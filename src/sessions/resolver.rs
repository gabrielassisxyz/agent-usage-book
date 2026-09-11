//! Project and repository resolution (`aub-lqe.12`, PLAN.md 19.3).
//!
//! Project and repository are typed logical identities, resolved through configured
//! aliases rather than embedded machine paths. Where a source provides a working
//! directory, the resolver maps it to the configured logical identity; where
//! nothing maps, the work lands in the unknown bucket and stays inside totals
//! rather than disappearing from them.
//!
//! The unknown buckets are ordinary keys, not `Option`: a report grouped by project
//! shows `unknown-project` as a visible group, which is what makes an unmapped
//! session distinguishable from a missing one.

use crate::config::AliasTable;

/// The logical project identity a report groups by.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ProjectKey(String);

impl ProjectKey {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The logical repository identity a report groups by.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RepositoryKey(String);

impl RepositoryKey {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The bucket unmapped work lands in; it stays inside totals.
pub const UNKNOWN_PROJECT: &str = "unknown-project";
/// The bucket unmapped work lands in; it stays inside totals.
pub const UNKNOWN_REPOSITORY: &str = "unknown-repository";

/// Resolves a working directory to its configured project identity, or to the
/// unknown bucket when the directory is unmapped or absent.
pub fn resolve_project(aliases: &AliasTable, working_dir: Option<&str>) -> ProjectKey {
    match working_dir.and_then(|dir| aliases.resolve(dir)) {
        Some(name) => ProjectKey::new(name),
        None => ProjectKey::new(UNKNOWN_PROJECT),
    }
}

/// Resolves a working directory to its configured repository identity, or to the
/// unknown bucket when the directory is unmapped or absent.
pub fn resolve_repository(aliases: &AliasTable, working_dir: Option<&str>) -> RepositoryKey {
    match working_dir.and_then(|dir| aliases.resolve(dir)) {
        Some(name) => RepositoryKey::new(name),
        None => RepositoryKey::new(UNKNOWN_REPOSITORY),
    }
}

/// The first stated working directory per session, in (source, native session
/// id) order, with the count of sessions where a later record disagreed.
///
/// A transcript may state a different directory mid-session (Claude Code
/// writes `cwd` on every line); the session keeps the first non-empty value
/// and every later differing non-empty value marks the session as changed
/// once, no matter how many lines disagree. The change count is the auditable
/// witness that the choice was made: it travels in the ingest summary as
/// `working_directory_changes`.
pub fn first_working_directories(
    observed: impl IntoIterator<Item = ((String, String), Option<String>)>,
) -> (
    std::collections::BTreeMap<(String, String), Option<String>>,
    u64,
) {
    let mut first: std::collections::BTreeMap<(String, String), Option<String>> =
        std::collections::BTreeMap::new();
    let mut changed: std::collections::BTreeSet<(String, String)> =
        std::collections::BTreeSet::new();
    for (session, directory) in observed {
        let directory = directory.filter(|dir| !dir.is_empty());
        match first.get(&session) {
            None => {
                first.insert(session, directory);
            }
            Some(stored) => match (stored, &directory) {
                // A session first seen without a directory adopts the first
                // stated one; that adoption is not a disagreement.
                (None, Some(_)) => {
                    first.insert(session, directory);
                }
                (Some(kept), Some(stated)) if kept != stated => {
                    changed.insert(session.clone());
                }
                _ => {}
            },
        }
    }
    let changes = changed.len() as u64;
    (first, changes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn aliases(pairs: &[(&str, &str)]) -> AliasTable {
        AliasTable::new(
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        )
        .unwrap()
    }

    #[test]
    fn mapped_directory_resolves_to_the_logical_identity() {
        let table = aliases(&[("/home/u/work/aub", "agent-usage-book")]);
        assert_eq!(
            resolve_project(&table, Some("/home/u/work/aub")).as_str(),
            "agent-usage-book"
        );
        assert_eq!(
            resolve_repository(&table, Some("/home/u/work/aub")).as_str(),
            "agent-usage-book"
        );
    }

    #[test]
    fn unmapped_directory_lands_in_the_unknown_bucket() {
        let table = aliases(&[("/home/u/work/aub", "agent-usage-book")]);
        assert_eq!(
            resolve_project(&table, Some("/home/u/work/elsewhere")).as_str(),
            UNKNOWN_PROJECT
        );
        assert_eq!(
            resolve_repository(&table, Some("/home/u/work/elsewhere")).as_str(),
            UNKNOWN_REPOSITORY
        );
    }

    #[test]
    fn absent_working_directory_lands_in_the_unknown_bucket() {
        let table = aliases(&[("/home/u/work/aub", "agent-usage-book")]);
        assert_eq!(resolve_project(&table, None).as_str(), UNKNOWN_PROJECT);
        assert_eq!(
            resolve_repository(&table, None).as_str(),
            UNKNOWN_REPOSITORY
        );
    }

    #[test]
    fn project_and_repository_resolve_independently() {
        let projects = aliases(&[("/p", "proj-a")]);
        let repositories = aliases(&[("/p", "repo-b")]);
        assert_eq!(resolve_project(&projects, Some("/p")).as_str(), "proj-a");
        assert_eq!(
            resolve_repository(&repositories, Some("/p")).as_str(),
            "repo-b"
        );
    }

    #[test]
    fn unknown_buckets_are_ordinary_keys_not_options() {
        // The bucket is a value like any other, so a grouping can show it.
        let mut keys = BTreeMap::new();
        keys.insert(ProjectKey::new(UNKNOWN_PROJECT), 1usize);
        keys.insert(ProjectKey::new("agent-usage-book"), 2usize);
        assert_eq!(keys.len(), 2);
    }

    fn observed(pairs: &[((&str, &str), Option<&str>)]) -> Vec<((String, String), Option<String>)> {
        pairs
            .iter()
            .map(|((source, native), dir)| {
                (
                    (source.to_string(), native.to_string()),
                    dir.map(str::to_string),
                )
            })
            .collect()
    }

    /// The first stated directory wins and a later disagreement is counted
    /// once per session, however many lines disagree.
    #[test]
    fn first_stated_directory_wins_and_a_disagreement_counts_once() {
        let (first, changes) = first_working_directories(observed(&[
            (("claude-code", "s1"), Some("/tmp/aub-fixture-project")),
            (
                ("claude-code", "s1"),
                Some("/tmp/aub-fixture-project-moved"),
            ),
            (
                ("claude-code", "s1"),
                Some("/tmp/aub-fixture-project-moved"),
            ),
        ]));
        assert_eq!(
            first.get(&("claude-code".to_string(), "s1".to_string())),
            Some(&Some("/tmp/aub-fixture-project".to_string()))
        );
        assert_eq!(changes, 1, "one session disagreed, counted once");
    }

    /// A session seen first without a directory adopts the first stated one,
    /// and that adoption is not a disagreement.
    #[test]
    fn a_session_first_seen_without_a_directory_adopts_the_first_stated_one() {
        let (first, changes) = first_working_directories(observed(&[
            (("pi", "s1"), None),
            (("pi", "s1"), Some("/tmp/aub-fixture-project")),
        ]));
        assert_eq!(
            first
                .get(&("pi".to_string(), "s1".to_string()))
                .cloned()
                .flatten()
                .as_deref(),
            Some("/tmp/aub-fixture-project")
        );
        assert_eq!(changes, 0);
    }

    /// Sessions that agree, sessions never stated, and an empty string that
    /// reads as absent: none of them is a change.
    #[test]
    fn agreement_absence_and_empty_strings_are_not_changes() {
        let (first, changes) = first_working_directories(observed(&[
            (("codex", "steady"), Some("/tmp/aub-fixture-project")),
            (("codex", "steady"), Some("/tmp/aub-fixture-project")),
            (("codex", "absent"), None),
            (("codex", "absent"), None),
            (("codex", "empty"), Some("")),
        ]));
        assert_eq!(changes, 0);
        assert_eq!(
            first
                .get(&("codex".to_string(), "empty".to_string()))
                .cloned()
                .flatten(),
            None,
            "an empty directory reads as absent"
        );
    }

    /// Two sessions are counted independently: the count is sessions, not lines.
    #[test]
    fn the_change_count_is_sessions_not_lines() {
        let (_, changes) = first_working_directories(observed(&[
            (("claude-code", "s1"), Some("/a")),
            (("claude-code", "s1"), Some("/b")),
            (("claude-code", "s2"), Some("/a")),
            (("claude-code", "s2"), Some("/c")),
            (("claude-code", "s2"), Some("/d")),
        ]));
        assert_eq!(changes, 2);
    }
}
