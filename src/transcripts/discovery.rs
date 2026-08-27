//! Recursive transcript discovery beneath configured roots.
//!
//! The legacy tools used a non-recursive glob and therefore silently dropped
//! subagent transcripts: the totals stayed plausible, nothing errored, and the
//! missing usage was invisible because nothing named the files that were never
//! opened. This module recurses beneath every configured root, applies the
//! configured pattern at every depth, does not follow symlinks by default, and
//! honours a maximum walk depth whose exceedance is reported rather than
//! silently truncating the file set.
//!
//! The pattern matcher is deliberately minimal: `*` matches any run of
//! characters, `?` matches exactly one, and a leading `**/` component matches
//! zero components (so the config's `**/*.jsonl` matches a bare file name at
//! every depth). A pattern that needs anything else a glob could mean is
//! rejected as unsupported rather than silently mis-matched.
//!
//! May not depend on:
//! - calibration
//! - cost models
//! - subscription window capacity, API pricing, task history, or meter
//!   percentages

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::config::TranscriptConfig;

/// Options controlling the walk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveryOptions {
    /// Follow directory symlinks. Off by default: a link into a large tree
    /// would turn discovery into a filesystem walk, and a cycle would never
    /// terminate. When on, cycles are still detected and skipped.
    pub follow_symlinks: bool,
    /// The maximum directory depth to descend into. A directory at this depth
    /// is not descended, and the cut-off directory is reported in
    /// [`SourceDiscovery::depth_exceeded`] rather than silently dropped.
    pub max_depth: usize,
}

impl Default for DiscoveryOptions {
    fn default() -> Self {
        Self {
            follow_symlinks: false,
            max_depth: 16,
        }
    }
}

/// The outcome of discovery over one configured source.
///
/// A source with no matching files is still present, with an empty `files`
/// list, so a zero is visible rather than producing an empty report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceDiscovery {
    /// The configured source name.
    pub source: String,
    /// The discovered files, in deterministic order.
    pub files: Vec<PathBuf>,
    /// Directories that were not descended into because they exceeded
    /// `max_depth`. Reported rather than silently truncating the file set.
    pub depth_exceeded: Vec<PathBuf>,
}

/// Why discovery failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoveryError {
    /// The configured root does not exist or is not a directory.
    RootMissing { source: String, path: PathBuf },
    /// The configured pattern uses a feature the minimal matcher does not
    /// support.
    UnsupportedPattern { source: String, pattern: String },
    /// A directory in the walk could not be read.
    UnreadableDirectory { path: PathBuf },
}

/// Discover transcript files beneath every configured root.
///
/// A root that does not exist is an error naming the source and the path,
/// never an empty result.
pub fn discover(
    sources: &[TranscriptConfig],
    options: &DiscoveryOptions,
) -> Result<Vec<SourceDiscovery>, DiscoveryError> {
    sources
        .iter()
        .map(|source| discover_source(source, options))
        .collect()
}

fn discover_source(
    source: &TranscriptConfig,
    options: &DiscoveryOptions,
) -> Result<SourceDiscovery, DiscoveryError> {
    let root = &source.root;
    if !root.is_dir() {
        return Err(DiscoveryError::RootMissing {
            source: source.name.clone(),
            path: root.clone(),
        });
    }
    let pattern = GlobPattern::compile(&source.name, &source.pattern)?;
    let mut files = Vec::new();
    let mut depth_exceeded = Vec::new();
    let mut visited = HashSet::new();
    walk(
        root,
        &pattern,
        options,
        0,
        &mut files,
        &mut depth_exceeded,
        &mut visited,
    )?;
    files.sort();
    depth_exceeded.sort();
    Ok(SourceDiscovery {
        source: source.name.clone(),
        files,
        depth_exceeded,
    })
}

/// Walk one directory, applying the pattern to every entry at every depth.
fn walk(
    dir: &Path,
    pattern: &GlobPattern,
    options: &DiscoveryOptions,
    depth: usize,
    files: &mut Vec<PathBuf>,
    depth_exceeded: &mut Vec<PathBuf>,
    visited: &mut HashSet<PathBuf>,
) -> Result<(), DiscoveryError> {
    if options.follow_symlinks {
        // Canonical paths resolve symlinks, so a cycle revisits a path already
        // in the set and is skipped instead of looping forever.
        let canonical = fs::canonicalize(dir).map_err(|_| DiscoveryError::UnreadableDirectory {
            path: dir.to_path_buf(),
        })?;
        if !visited.insert(canonical) {
            return Ok(());
        }
    }

    let mut entries: Vec<_> = fs::read_dir(dir)
        .map_err(|_| DiscoveryError::UnreadableDirectory {
            path: dir.to_path_buf(),
        })?
        .collect::<Result<_, _>>()
        .map_err(|_| DiscoveryError::UnreadableDirectory {
            path: dir.to_path_buf(),
        })?;
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|_| DiscoveryError::UnreadableDirectory { path: path.clone() })?;

        if file_type.is_dir() {
            descend(
                &path,
                pattern,
                options,
                depth,
                files,
                depth_exceeded,
                visited,
            )?;
        } else if file_type.is_file() {
            if pattern.matches(&path) {
                files.push(path);
            }
        } else if file_type.is_symlink() && options.follow_symlinks {
            // Resolve the link and treat the target as a file or a directory.
            let target_type = fs::metadata(&path)
                .map_err(|_| DiscoveryError::UnreadableDirectory { path: path.clone() })?;
            if target_type.is_dir() {
                descend(
                    &path,
                    pattern,
                    options,
                    depth,
                    files,
                    depth_exceeded,
                    visited,
                )?;
            } else if target_type.is_file() && pattern.matches(&path) {
                files.push(path);
            }
        }
        // A symlink with follow_symlinks off is not followed at all.
    }
    Ok(())
}

/// Descend into a directory, honouring the depth bound and reporting the
/// cut-off rather than silently truncating.
fn descend(
    dir: &Path,
    pattern: &GlobPattern,
    options: &DiscoveryOptions,
    depth: usize,
    files: &mut Vec<PathBuf>,
    depth_exceeded: &mut Vec<PathBuf>,
    visited: &mut HashSet<PathBuf>,
) -> Result<(), DiscoveryError> {
    if depth >= options.max_depth {
        depth_exceeded.push(dir.to_path_buf());
        return Ok(());
    }
    walk(
        dir,
        pattern,
        options,
        depth + 1,
        files,
        depth_exceeded,
        visited,
    )
}

/// A compiled glob pattern, deliberately minimal.
///
/// `*` matches any run of characters, `?` matches exactly one, and a leading
/// `**/` component matches zero components, so the config's `**/*.jsonl`
/// matches a bare file name at every depth. Character classes, braces,
/// escaping and embedded path separators are rejected as unsupported rather
/// than silently mis-matched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlobPattern {
    name_pattern: String,
}

impl GlobPattern {
    fn compile(source: &str, pattern: &str) -> Result<Self, DiscoveryError> {
        let mut name_pattern = pattern;
        while let Some(rest) = name_pattern.strip_prefix("**/") {
            name_pattern = rest;
        }
        if name_pattern.contains('/') {
            return Err(DiscoveryError::UnsupportedPattern {
                source: source.to_string(),
                pattern: pattern.to_string(),
            });
        }
        for c in name_pattern.chars() {
            if matches!(c, '[' | ']' | '{' | '}' | '\\') {
                return Err(DiscoveryError::UnsupportedPattern {
                    source: source.to_string(),
                    pattern: pattern.to_string(),
                });
            }
        }
        Ok(Self {
            name_pattern: name_pattern.to_string(),
        })
    }

    /// Whether the pattern matches the file name of `path`.
    fn matches(&self, path: &Path) -> bool {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        glob_match(&self.name_pattern, name)
    }
}

/// Greedy glob matching over `*` and `?`, linear in the name length.
fn glob_match(pattern: &str, name: &str) -> bool {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn config(name: &str, root: &Path, pattern: &str) -> TranscriptConfig {
        TranscriptConfig {
            name: name.to_string(),
            root: root.to_path_buf(),
            pattern: pattern.to_string(),
            usage_evidence: None,
        }
    }

    /// A scratch tree unique to one test, removed before and after.
    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aub-lqe1-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    /// The nested-only fixture: every transcript file lives in a subdirectory,
    /// so a flat (root-only) glob finds zero and only recursion finds the
    /// total. This is the regression the bead exists for: a reversion to a
    /// flat glob makes this test fail by returning zero.
    #[test]
    fn nested_only_fixture_produces_a_nonzero_total() {
        let root =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/transcripts/nested-only");
        let sources = [config("nested", &root, "*.jsonl")];
        let result = discover(&sources, &DiscoveryOptions::default()).unwrap();
        assert_eq!(result.len(), 1);
        assert!(
            !result[0].files.is_empty(),
            "a flat glob would find zero files in the nested-only fixture"
        );
        assert!(
            result[0]
                .files
                .iter()
                .any(|path| path.components().count() > root.components().count() + 1),
            "the fixture's files must be nested, not at the root"
        );
    }

    /// Symlinks are not followed by default, including a symlink cycle that
    /// would otherwise never terminate.
    #[cfg(unix)]
    #[test]
    fn symlinks_are_not_followed_by_default() {
        use std::os::unix::fs::symlink;
        let root = scratch("symlink");
        let real = root.join("real");
        write(&real.join("a.jsonl"), "{}");
        // A symlink to a directory and a symlink cycle.
        symlink(&real, root.join("link-to-real")).unwrap();
        symlink(
            root.join("link-to-real"),
            root.join("link-to-real").join("cycle"),
        )
        .unwrap();

        let sources = [config("s", &root, "*.jsonl")];
        let result = discover(&sources, &DiscoveryOptions::default()).unwrap();
        assert_eq!(
            result[0].files,
            vec![real.join("a.jsonl")],
            "the symlinked copy must not be discovered by default"
        );
    }

    /// Following symlinks is an explicit opt-in, and a cycle still terminates.
    #[cfg(unix)]
    #[test]
    fn following_symlinks_is_an_explicit_opt_in() {
        use std::os::unix::fs::symlink;
        let root = scratch("follow");
        let real = root.join("real");
        write(&real.join("a.jsonl"), "{}");
        symlink(&real, root.join("link-to-real")).unwrap();
        symlink(
            root.join("link-to-real"),
            root.join("link-to-real").join("cycle"),
        )
        .unwrap();

        let options = DiscoveryOptions {
            follow_symlinks: true,
            ..DiscoveryOptions::default()
        };
        let sources = [config("s", &root, "*.jsonl")];
        let result = discover(&sources, &options).unwrap();
        assert_eq!(
            result[0].files.len(),
            1,
            "the symlinked copy is discovered once, and the cycle terminates"
        );
    }

    /// The maximum walk depth is honoured, and exceeding it is reported
    /// rather than silently truncating the file set.
    #[test]
    fn max_depth_is_honoured_and_reported() {
        let root = scratch("depth");
        write(&root.join("top.jsonl"), "{}");
        write(&root.join("a").join("mid.jsonl"), "{}");
        write(&root.join("a").join("b").join("deep.jsonl"), "{}");

        let options = DiscoveryOptions {
            max_depth: 1,
            ..DiscoveryOptions::default()
        };
        let sources = [config("s", &root, "*.jsonl")];
        let result = discover(&sources, &options).unwrap();
        assert_eq!(
            result[0].files,
            vec![root.join("a").join("mid.jsonl"), root.join("top.jsonl")],
            "depth 1 reaches the root and one level of subdirectories"
        );
        assert_eq!(
            result[0].depth_exceeded,
            vec![root.join("a").join("b")],
            "the cut-off directory is reported, not silently dropped"
        );
    }

    /// The configured glob is applied at every depth, not only at the root.
    #[test]
    fn the_glob_is_applied_at_every_depth() {
        let root = scratch("glob");
        write(&root.join("root.jsonl"), "{}");
        write(&root.join("sub").join("nested.jsonl"), "{}");
        write(&root.join("sub").join("deep").join("deepest.jsonl"), "{}");
        write(&root.join("sub").join("ignored.txt"), "not a transcript");

        let sources = [config("s", &root, "*.jsonl")];
        let result = discover(&sources, &DiscoveryOptions::default()).unwrap();
        assert_eq!(
            result[0].files.len(),
            3,
            "the pattern matches at every depth"
        );
        assert!(
            result[0]
                .files
                .iter()
                .all(|path| path.extension().and_then(|e| e.to_str()) == Some("jsonl")),
            "non-matching files must be filtered at every depth"
        );
    }

    /// A root that does not exist is an error naming the source and the path,
    /// never an empty result.
    #[test]
    fn a_missing_root_is_an_error_naming_source_and_path() {
        let root = scratch("missing").join("does-not-exist");
        let sources = [config("cli-a", &root, "*.jsonl")];
        let err = discover(&sources, &DiscoveryOptions::default()).unwrap_err();
        assert_eq!(
            err,
            DiscoveryError::RootMissing {
                source: "cli-a".to_string(),
                path: root.clone(),
            }
        );
    }

    /// A pattern the minimal matcher cannot honour is rejected rather than
    /// silently mis-matched.
    #[test]
    fn an_unsupported_pattern_is_rejected() {
        let root = scratch("pattern");
        let sources = [config("s", &root, "session-[0-9].jsonl")];
        let err = discover(&sources, &DiscoveryOptions::default()).unwrap_err();
        assert_eq!(
            err,
            DiscoveryError::UnsupportedPattern {
                source: "s".to_string(),
                pattern: "session-[0-9].jsonl".to_string(),
            }
        );
    }

    /// The `**/` prefix matches zero components, so the config's own pattern
    /// shape works at every depth.
    #[test]
    fn a_double_star_prefix_matches_zero_components() {
        let root = scratch("doublestar");
        write(&root.join("root.jsonl"), "{}");
        write(&root.join("sub").join("nested.jsonl"), "{}");

        let sources = [config("s", &root, "**/*.jsonl")];
        let result = discover(&sources, &DiscoveryOptions::default()).unwrap();
        assert_eq!(result[0].files.len(), 2);
    }

    /// Discovery over a synthetic tree of stated size completes within its
    /// budget: a deep root is the case that would otherwise walk a filesystem.
    #[test]
    fn discovery_over_a_synthetic_tree_completes_within_budget() {
        let root = scratch("perf");
        for level in 0..20 {
            for branch in 0..5 {
                let dir = root.join(format!("l{level}")).join(format!("b{branch}"));
                write(&dir.join(format!("t-{level}-{branch}.jsonl")), "{}");
            }
        }
        let sources = [config("s", &root, "*.jsonl")];
        let started = Instant::now();
        let result = discover(&sources, &DiscoveryOptions::default()).unwrap();
        let elapsed = started.elapsed();
        assert_eq!(result[0].files.len(), 100, "20 levels x 5 branches");
        assert!(
            elapsed.as_secs() < 5,
            "discovery of 100 files took {elapsed:?}, over the 5s budget"
        );
    }
}
