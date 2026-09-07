//! The local-file source behind [`crate::meter::transport::LocalFile`].
//!
//! A file-backed meter (the Codex rollout, `aub-cg6k`) gets its bytes through
//! the same transport port an HTTP meter does, so the evidence capture and the
//! synthetic transport see one shape. The disk reads themselves live here,
//! outside `src/meter/`, because boundary rule 17 forbids the meter module every
//! `std::fs` facility and reads the module's source as text: the transport
//! names this module's one entry point and touches no file itself.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::domain::failure::FailureClass;
use crate::domain::time::Clock;
use crate::meter::transport::{
    CommandBudget, HttpResponse, LOCAL_FILE_MTIME_HEADER, LOCAL_FILE_PATH_HEADER, LocalFile,
};

/// Serves one local-file request from disk: the real arm of the
/// [`LocalFile`] source (`aub-cg6k`). The resolved path and the file's
/// modification time ride the response headers under
/// [`LOCAL_FILE_PATH_HEADER`] and [`LOCAL_FILE_MTIME_HEADER`], the same role
/// an HTTP server's own location and last-modified headers play.
///
/// A source that names nothing to read is [`FailureClass::MalformedBody`]:
/// the shared vocabulary has no separate local-IO class, and a local source
/// with nothing to read is the same outcome an empty provider body is - the
/// adapter's no-evidence state - never a silently substituted zero.
pub(crate) fn serve_local_file(
    local: &LocalFile,
    budget: &CommandBudget,
    clock: &impl Clock,
) -> Result<HttpResponse, FailureClass> {
    if budget.is_expired(clock) {
        return Err(FailureClass::TotalBudgetExpired);
    }
    let (path, modified) = match &local.newest_glob {
        Some(pattern) => {
            newest_matching_file(&local.path, pattern).ok_or(FailureClass::MalformedBody)?
        }
        None => {
            let metadata =
                std::fs::metadata(&local.path).map_err(|_| FailureClass::MalformedBody)?;
            let modified = metadata
                .modified()
                .map_err(|_| FailureClass::MalformedBody)?;
            (local.path.clone(), modified)
        }
    };
    let body = std::fs::read(&path).map_err(|_| FailureClass::MalformedBody)?;
    if budget.is_expired(clock) {
        return Err(FailureClass::TotalBudgetExpired);
    }
    Ok(HttpResponse {
        status: 200,
        headers: vec![
            (
                LOCAL_FILE_PATH_HEADER.to_string(),
                path.display().to_string(),
            ),
            (
                LOCAL_FILE_MTIME_HEADER.to_string(),
                system_time_to_unix_nanos(modified)
                    .ok_or(FailureClass::MalformedBody)?
                    .to_string(),
            ),
        ],
        body,
    })
}

/// Unix-epoch nanoseconds for a filesystem modification time, or `None` for
/// an instant before the epoch, which no real file has and no reading could
/// justify.
fn system_time_to_unix_nanos(time: SystemTime) -> Option<i64> {
    let nanos = i64::try_from(time.duration_since(UNIX_EPOCH).ok()?.as_nanos()).ok()?;
    Some(nanos)
}

/// The newest file under `root` whose file name matches `pattern`, searched
/// recursively, by modification time. Ties break on the greater path, so the
/// answer never depends on directory iteration order. Entries this process
/// cannot read or stat are skipped: a partially readable tree still yields
/// the newest rollout it actually contains, and a tree with no readable
/// match at all leaves the caller's own no-evidence handling in charge.
fn newest_matching_file(root: &Path, pattern: &str) -> Option<(PathBuf, SystemTime)> {
    let mut newest: Option<(PathBuf, SystemTime)> = None;
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        // An unreadable subdirectory is skipped, not fatal: a partially
        // readable tree still yields the newest rollout it contains.
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            // Symlinked directories are never followed: a cycle would make
            // the walk unbounded, and no rollout tree needs one.
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                pending.push(path);
                continue;
            }
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if !local_file_glob_match(pattern, name) {
                continue;
            }
            let Ok(metadata) = std::fs::metadata(&path) else {
                continue;
            };
            let Ok(modified) = metadata.modified() else {
                continue;
            };
            let takes_place = match &newest {
                Some((newest_path, newest_modified)) => {
                    modified > *newest_modified
                        || (modified == *newest_modified && path > *newest_path)
                }
                None => true,
            };
            if takes_place {
                newest = Some((path, modified));
            }
        }
    }
    newest
}

/// Greedy glob matching over `*` and `?`, linear in the name length. It
/// mirrors the matcher in `src/transcripts/discovery.rs` because both match
/// file-name globs, and it is deliberately not shared with it: the meter
/// boundary may not depend on the transcript modules, and a one-screen
/// matcher does not justify coupling two provider-facing layers together.
fn local_file_glob_match(pattern: &str, name: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let name: Vec<char> = name.chars().collect();
    let (mut pi, mut ni) = (0usize, 0usize);
    let mut star: Option<usize> = None;
    let mut mark = 0usize;
    while ni < name.len() {
        if pi < pattern.len() && (pattern[pi] == '?' || pattern[pi] == name[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < pattern.len() && pattern[pi] == '*' {
            star = Some(pi);
            mark = ni;
            pi += 1;
        } else if let Some(star_pos) = star {
            pi = star_pos + 1;
            mark += 1;
            ni = mark;
        } else {
            return false;
        }
    }
    while pi < pattern.len() && pattern[pi] == '*' {
        pi += 1;
    }
    pi == pattern.len()
}
