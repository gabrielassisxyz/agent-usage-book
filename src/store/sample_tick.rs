//! The last `aub sample` invocation's outcome, recorded outside the SQLite
//! ledger (`aub-va6s`).
//!
//! A tick refused by a locked database is exactly the tick whose own row
//! never lands in `sample_run`: the ledger cannot record what it could not
//! write to. Without a record that lives somewhere else, a run of refused
//! ticks is visible only in the scheduler's own journal, which `aub doctor`
//! does not read and an operator does not check until the gap is already
//! old. This module is that somewhere else: a small file inside the state
//! directory, written unconditionally at the end of every sampling attempt,
//! success or failure, so `doctor` can report the last tick's outcome from a
//! plain read that never depends on the ledger being reachable.
//!
//! May not depend on:
//! - presentation
//! - provider adapters

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::domain::time::UtcTimestamp;
use crate::error::Error;
use crate::store::startup::ensure_dir_mode_0700;

/// The file name of the last-tick marker inside the state directory.
pub const LAST_SAMPLE_TICK_FILE_NAME: &str = "sample-tick.json";

const SCHEMA_VERSION: u32 = 1;

/// What the most recent `aub sample` invocation ended as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TickOutcome {
    Success,
    /// `detail` is the failure's own message, already carrying how long the
    /// attempt waited when the failure was a locked database (`aub-va6s`
    /// names that duration at the point the error is built).
    Failed(String),
}

/// One recorded tick: when it started and how it ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastSampleTick {
    pub started_at: UtcTimestamp,
    pub outcome: TickOutcome,
}

/// The marker file's path inside a state directory.
pub fn last_sample_tick_path(state_dir: &Path) -> PathBuf {
    state_dir.join(LAST_SAMPLE_TICK_FILE_NAME)
}

/// Records `tick` as the last sample tick, replacing whatever was recorded
/// before. Write-then-rename, so a reader never observes a torn file.
pub fn record_last_tick(state_dir: &Path, tick: &LastSampleTick) -> Result<(), Error> {
    ensure_dir_mode_0700(state_dir)?;
    let (outcome_label, detail) = match &tick.outcome {
        TickOutcome::Success => ("success", None),
        TickOutcome::Failed(reason) => ("failed", Some(reason.as_str())),
    };
    let document = json!({
        "schema_version": SCHEMA_VERSION,
        "started_at_nanos": tick.started_at.unix_nanos(),
        "outcome": outcome_label,
        "detail": detail,
    });
    atomic_write(
        &last_sample_tick_path(state_dir),
        document.to_string().as_bytes(),
    )
}

/// Reads the last recorded tick, or `None` when no tick has been recorded yet
/// (a fresh state directory, or one from before this bead).
pub fn read_last_tick(state_dir: &Path) -> Result<Option<LastSampleTick>, Error> {
    let path = last_sample_tick_path(state_dir);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(Error::Store(format!(
                "cannot read the last sample tick at {path:?}: {error}"
            )));
        }
    };
    let document: Value = serde_json::from_str(&text).map_err(|error| {
        Error::Store(format!(
            "cannot parse the last sample tick {path:?}: {error}"
        ))
    })?;
    let started_at_nanos = document
        .get("started_at_nanos")
        .and_then(Value::as_i64)
        .ok_or_else(|| Error::Store(format!("{path:?} carries no started_at_nanos field")))?;
    let outcome = match document.get("outcome").and_then(Value::as_str) {
        Some("success") => TickOutcome::Success,
        Some("failed") => {
            let detail = document
                .get("detail")
                .and_then(Value::as_str)
                .unwrap_or("(no detail recorded)")
                .to_string();
            TickOutcome::Failed(detail)
        }
        other => {
            return Err(Error::Store(format!(
                "{path:?} carries unknown outcome {other:?}"
            )));
        }
    };
    Ok(Some(LastSampleTick {
        started_at: UtcTimestamp::from_unix_nanos(started_at_nanos),
        outcome,
    }))
}

/// Writes `bytes` to `target` so a reader never observes a torn file, and so
/// the file exists at mode 0600 from the moment it first appears. Mirrors
/// `crate::projection`'s own file publication; duplicated rather than shared
/// because the two files have no other relationship and sharing the helper
/// would reach across a module boundary for twenty lines.
fn atomic_write(target: &Path, bytes: &[u8]) -> Result<(), Error> {
    let parent = target.parent().unwrap_or_else(|| Path::new(""));
    let file_name = target
        .file_name()
        .ok_or_else(|| Error::Store(format!("sample tick target {target:?} has no file name")))?
        .to_string_lossy()
        .into_owned();
    let temporary = parent.join(format!("{file_name}.tmp-{}", std::process::id()));

    let mut file = open_temporary(&temporary)?;
    file.write_all(bytes).map_err(|error| {
        Error::Store(format!("cannot write the sample tick temporary: {error}"))
    })?;
    file.sync_all().map_err(|error| {
        Error::Store(format!("cannot fsync the sample tick temporary: {error}"))
    })?;
    drop(file);

    fs::rename(&temporary, target).map_err(|error| {
        let _ = fs::remove_file(&temporary);
        Error::Store(format!("cannot publish the sample tick file: {error}"))
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
                "aub-sample-tick-test-{}-{suffix}",
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
    fn a_missing_marker_reads_as_none_not_as_a_failure() {
        let scratch = ScratchDir::new();
        assert_eq!(read_last_tick(scratch.path()).unwrap(), None);
    }

    #[test]
    fn a_recorded_success_round_trips_exactly() {
        let scratch = ScratchDir::new();
        let tick = LastSampleTick {
            started_at: UtcTimestamp::from_unix_nanos(1_000),
            outcome: TickOutcome::Success,
        };
        record_last_tick(scratch.path(), &tick).unwrap();
        assert_eq!(read_last_tick(scratch.path()).unwrap(), Some(tick));
    }

    #[test]
    fn a_recorded_failure_round_trips_its_detail_exactly() {
        let scratch = ScratchDir::new();
        let tick = LastSampleTick {
            started_at: UtcTimestamp::from_unix_nanos(2_000),
            outcome: TickOutcome::Failed("database is locked (waited up to 5000ms)".to_string()),
        };
        record_last_tick(scratch.path(), &tick).unwrap();
        assert_eq!(read_last_tick(scratch.path()).unwrap(), Some(tick));
    }

    /// A second recording replaces the first, never appends: the marker
    /// carries only the *last* tick, so a reader never sees a success and a
    /// failure both claimed for a state directory with a real history of
    /// both.
    #[test]
    fn recording_a_second_tick_replaces_the_first_rather_than_appending() {
        let scratch = ScratchDir::new();
        record_last_tick(
            scratch.path(),
            &LastSampleTick {
                started_at: UtcTimestamp::from_unix_nanos(1_000),
                outcome: TickOutcome::Success,
            },
        )
        .unwrap();
        record_last_tick(
            scratch.path(),
            &LastSampleTick {
                started_at: UtcTimestamp::from_unix_nanos(2_000),
                outcome: TickOutcome::Failed("refused".to_string()),
            },
        )
        .unwrap();
        let read = read_last_tick(scratch.path()).unwrap().unwrap();
        assert_eq!(read.started_at, UtcTimestamp::from_unix_nanos(2_000));
        assert_eq!(read.outcome, TickOutcome::Failed("refused".to_string()));

        let raw = fs::read_to_string(last_sample_tick_path(scratch.path())).unwrap();
        assert_eq!(
            raw.matches("started_at_nanos").count(),
            1,
            "the marker holds one tick, not an appended history: {raw}"
        );
    }

    #[test]
    fn a_corrupt_marker_is_a_named_store_failure_not_a_silent_none() {
        let scratch = ScratchDir::new();
        fs::write(last_sample_tick_path(scratch.path()), b"not json").unwrap();
        let error = read_last_tick(scratch.path()).unwrap_err();
        assert!(
            matches!(error, Error::Store(ref message) if message.contains("cannot parse")),
            "{error}"
        );
    }
}
