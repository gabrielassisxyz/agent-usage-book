//! Project and repository resolution (`aub-lqe.12`, PLAN.md 19.3, `aub-p07j`).
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
//!
//! With `[layout]` configured (`aub-p07j`), resolution gains the checkout roots
//! below the explicit aliases. Precedence is: an explicit alias with an exact
//! key wins; then the `worktrees` root; then the `repositories` root; then
//! unknown. The worktree root is checked first because it sits inside the
//! repositories root on this machine. Under a root the identity is the first
//! path segment after it, a repository named in `ignore` resolves to unknown,
//! and a dot-named segment is refused at resolution (reported once per ingest
//! in the summary). A project is its repository unless an explicit `[projects]`
//! entry, matched exactly, says otherwise.

use crate::config::AliasTable;
use crate::config::layout::{LayoutRoots, layout_repository_name, valid_layout_repository_name};

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

/// Resolves a working directory to its repository identity through an explicit
/// alias table plus the configured layout roots (`aub-p07j`): an explicit
/// alias with an exact key wins; then the `worktrees` root; then the
/// `repositories` root; then unknown. A layout-derived name in `ignore`, or a
/// dot-named segment from a misconfigured root, resolves to unknown rather
/// than to a guessed or dot-named repository.
pub fn resolve_repository_with_layout(
    aliases: &AliasTable,
    layout: &LayoutRoots,
    working_dir: Option<&str>,
) -> RepositoryKey {
    let dir = match working_dir {
        Some(dir) if !dir.is_empty() => dir,
        _ => return RepositoryKey::new(UNKNOWN_REPOSITORY),
    };
    if let Some(name) = aliases.resolve(dir) {
        return RepositoryKey::new(name);
    }
    match layout_repository_name(layout, dir) {
        Some(name) if layout.is_ignored(&name) => RepositoryKey::new(UNKNOWN_REPOSITORY),
        Some(name) if valid_layout_repository_name(&name) => RepositoryKey::new(name),
        _ => RepositoryKey::new(UNKNOWN_REPOSITORY),
    }
}

/// Resolves a working directory to its project identity through an explicit
/// project table, an explicit repository table and the layout roots
/// (`aub-p07j`): an explicit `[projects]` entry with an exact key wins,
/// otherwise, when a layout root is set, the project is its repository as
/// resolved above, which is how a project spanning repositories is expressed.
/// With neither root set, an unmatched `[projects]` lookup stays unknown, as
/// it was before the layout existed.
pub fn resolve_project_with_layout(
    projects: &AliasTable,
    repositories: &AliasTable,
    layout: &LayoutRoots,
    working_dir: Option<&str>,
) -> ProjectKey {
    let dir = match working_dir {
        Some(dir) if !dir.is_empty() => dir,
        _ => return ProjectKey::new(UNKNOWN_PROJECT),
    };
    if let Some(name) = projects.resolve(dir) {
        return ProjectKey::new(name);
    }
    // Rule 6: with neither root set, the project resolves exactly as before,
    // through `[projects]` alone, never through a `[repositories]` alias.
    if !layout.has_root() {
        return ProjectKey::new(UNKNOWN_PROJECT);
    }
    let repository = resolve_repository_with_layout(repositories, layout, Some(dir));
    if repository.as_str() == UNKNOWN_REPOSITORY {
        ProjectKey::new(UNKNOWN_PROJECT)
    } else {
        ProjectKey::new(repository.as_str())
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

    /// The `aub-p07j` acceptance table, row by row: with
    /// `repositories = "/r"`, `worktrees = "/r/.worktrees"`,
    /// `ignore = ["scratch"]` and `[repositories] "/elsewhere/x" =
    /// "x-explicit"`, every listed working directory resolves to its listed
    /// repository and project.
    mod layout_acceptance {
        use super::*;
        use crate::config::layout::LayoutRoots;
        use std::path::PathBuf;

        fn tables() -> (AliasTable, AliasTable, LayoutRoots) {
            let projects = AliasTable::new(BTreeMap::new()).unwrap();
            let repositories = AliasTable::new(BTreeMap::from([(
                "/elsewhere/x".to_string(),
                "x-explicit".to_string(),
            )]))
            .unwrap();
            let layout = LayoutRoots {
                repositories: Some(PathBuf::from("/r")),
                worktrees: Some(PathBuf::from("/r/.worktrees")),
                ignore: vec!["scratch".to_string()],
            };
            (projects, repositories, layout)
        }

        fn check(dir: &str, repository: &str, project: &str) {
            let (projects, repositories, layout) = tables();
            assert_eq!(
                resolve_repository_with_layout(&repositories, &layout, Some(dir)).as_str(),
                repository,
                "repository for {dir:?}"
            );
            assert_eq!(
                resolve_project_with_layout(&projects, &repositories, &layout, Some(dir)).as_str(),
                project,
                "project for {dir:?}"
            );
        }

        #[test]
        fn repositories_root_names_the_immediate_child() {
            check("/r/aub", "aub", "aub");
            check("/r/aub/src/meter", "aub", "aub");
        }

        #[test]
        fn worktrees_root_names_the_repo_before_the_task() {
            check("/r/.worktrees/aub/bugfix-x", "aub", "aub");
            check("/r/.worktrees/aub/bugfix-x/src", "aub", "aub");
        }

        #[test]
        fn roots_themselves_and_ignored_and_outside_dirs_are_unknown() {
            check("/r/.worktrees", UNKNOWN_REPOSITORY, UNKNOWN_PROJECT);
            check("/r", UNKNOWN_REPOSITORY, UNKNOWN_PROJECT);
            check("/r/scratch/notes", UNKNOWN_REPOSITORY, UNKNOWN_PROJECT);
            check("/tmp/build", UNKNOWN_REPOSITORY, UNKNOWN_PROJECT);
        }

        /// Rule 6: with no `[layout]` roots, a `[repositories]` alias does not
        /// name the project; the project resolves through `[projects]` alone,
        /// exactly as `resolve_project` did before the layout existed.
        #[test]
        fn without_a_layout_root_a_repository_alias_does_not_name_the_project() {
            let projects = AliasTable::new(BTreeMap::new()).unwrap();
            let repositories =
                AliasTable::new(BTreeMap::from([("/w".to_string(), "repo".to_string())])).unwrap();
            let no_layout = LayoutRoots::default();
            assert_eq!(
                resolve_repository_with_layout(&repositories, &no_layout, Some("/w")).as_str(),
                "repo"
            );
            assert_eq!(
                resolve_project_with_layout(&projects, &repositories, &no_layout, Some("/w"))
                    .as_str(),
                UNKNOWN_PROJECT
            );
            assert_eq!(
                resolve_project_with_layout(&projects, &repositories, &no_layout, Some("/w"))
                    .as_str(),
                resolve_project(&projects, Some("/w")).as_str(),
                "matches the pre-layout resolver"
            );
        }

        /// A working directory equal to a configured root resolves to unknown
        /// and raises no misconfiguration warning: the worktrees root does not
        /// fall through to the repositories root as `.worktrees`.
        #[test]
        fn a_configured_root_itself_is_unknown_without_a_warning() {
            let (_, _, layout) = tables();
            for dir in ["/r", "/r/.worktrees"] {
                check(dir, UNKNOWN_REPOSITORY, UNKNOWN_PROJECT);
                assert_eq!(
                    crate::config::layout::layout_rejected_repository_name(&layout, dir),
                    None,
                    "no layout_rejected entry for the root {dir:?}"
                );
            }
        }

        #[test]
        fn an_explicit_alias_wins_over_the_roots() {
            check("/elsewhere/x", "x-explicit", "x-explicit");
        }

        #[test]
        fn absent_working_directory_is_unknown_under_layout_too() {
            let (projects, repositories, layout) = tables();
            assert_eq!(
                resolve_repository_with_layout(&repositories, &layout, None).as_str(),
                UNKNOWN_REPOSITORY
            );
            assert_eq!(
                resolve_project_with_layout(&projects, &repositories, &layout, None).as_str(),
                UNKNOWN_PROJECT
            );
            assert_eq!(
                resolve_project_with_layout(&projects, &repositories, &layout, Some("")).as_str(),
                UNKNOWN_PROJECT
            );
        }

        /// With `[projects] "/r/aub" = "usage"` added, `/r/aub` resolves to
        /// repository `aub` and project `usage`, while `/r/aub/src` resolves
        /// to project `aub`: the project alias is an exact key, never a
        /// prefix.
        #[test]
        fn an_explicit_project_overrides_the_repository_default_exactly() {
            let (_, repositories, layout) = tables();
            let projects = AliasTable::new(BTreeMap::from([(
                "/r/aub".to_string(),
                "usage".to_string(),
            )]))
            .unwrap();
            assert_eq!(
                resolve_repository_with_layout(&repositories, &layout, Some("/r/aub")).as_str(),
                "aub"
            );
            assert_eq!(
                resolve_project_with_layout(&projects, &repositories, &layout, Some("/r/aub"))
                    .as_str(),
                "usage"
            );
            assert_eq!(
                resolve_project_with_layout(&projects, &repositories, &layout, Some("/r/aub/src"))
                    .as_str(),
                "aub",
                "a subdirectory does not inherit the exact project key"
            );
        }

        /// Property: any path under `<worktrees>/<repo>/...` with a
        /// non-empty, slash-free `repo` resolves to `repo`.
        #[test]
        fn any_worktree_path_resolves_to_its_repo_segment() {
            let (projects, repositories, layout) = tables();
            for repo in ["aub", "x", "repo-with-dashes", "UPPER", "0"] {
                for suffix in ["", "/task", "/task/src/deep"] {
                    let dir = format!("/r/.worktrees/{repo}{suffix}");
                    assert_eq!(
                        resolve_repository_with_layout(&repositories, &layout, Some(&dir)).as_str(),
                        repo,
                        "worktree path {dir:?}"
                    );
                    assert_eq!(
                        resolve_project_with_layout(&projects, &repositories, &layout, Some(&dir))
                            .as_str(),
                        repo,
                        "worktree project {dir:?}"
                    );
                }
            }
        }

        /// Property: any path under `<repositories>/<repo>/...` with a
        /// non-empty, slash-free `repo` outside `ignore` resolves to `repo`.
        #[test]
        fn any_repository_path_resolves_to_its_first_segment() {
            let (projects, repositories, layout) = tables();
            for repo in ["aub", "other"] {
                for suffix in ["", "/src", "/src/meter"] {
                    let dir = format!("/r/{repo}{suffix}");
                    assert_eq!(
                        resolve_repository_with_layout(&repositories, &layout, Some(&dir)).as_str(),
                        repo,
                        "repository path {dir:?}"
                    );
                    assert_eq!(
                        resolve_project_with_layout(&projects, &repositories, &layout, Some(&dir))
                            .as_str(),
                        repo,
                        "repository project {dir:?}"
                    );
                }
            }
        }

        /// Property: any path not under either root and not an explicit key
        /// resolves to unknown.
        #[test]
        fn paths_outside_both_roots_are_unknown() {
            let (projects, repositories, layout) = tables();
            for dir in [
                "/tmp/build",
                "/home/u/work/aub",
                "/r2/aub",
                "/r-suffix/aub",
                "/",
            ] {
                assert_eq!(
                    resolve_repository_with_layout(&repositories, &layout, Some(dir)).as_str(),
                    UNKNOWN_REPOSITORY,
                    "outside path {dir:?}"
                );
                assert_eq!(
                    resolve_project_with_layout(&projects, &repositories, &layout, Some(dir))
                        .as_str(),
                    UNKNOWN_PROJECT,
                    "outside project {dir:?}"
                );
            }
        }

        /// A dot-named segment from a misconfigured root is refused at
        /// resolution: with no worktrees root, the worktrees directory itself
        /// appears as `.worktrees` and must land in unknown, never as a
        /// dot-named repository.
        #[test]
        fn dot_named_segments_are_refused_at_resolution() {
            let projects = AliasTable::new(BTreeMap::new()).unwrap();
            let repositories = AliasTable::new(BTreeMap::new()).unwrap();
            let repos_only = LayoutRoots {
                repositories: Some(PathBuf::from("/r")),
                worktrees: None,
                ignore: Vec::new(),
            };
            assert_eq!(
                resolve_repository_with_layout(
                    &repositories,
                    &repos_only,
                    Some("/r/.worktrees/aub/x")
                )
                .as_str(),
                UNKNOWN_REPOSITORY
            );
            assert_eq!(
                resolve_project_with_layout(
                    &projects,
                    &repositories,
                    &repos_only,
                    Some("/r/.worktrees/aub/x")
                )
                .as_str(),
                UNKNOWN_PROJECT
            );
        }
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
