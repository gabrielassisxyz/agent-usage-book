//! The `[layout]` repository roots (`aub-p07j`).
//!
//! Two configured roots describe the whole checkout layout instead of one alias
//! per checkout: `repositories` makes every immediate child directory a
//! repository named after that directory, and `worktrees` makes
//! `<dir>/<repo>/<anything>` resolve to `<repo>`. `ignore` sends named
//! repositories to the unknown bucket on purpose. Explicit `[repositories]` /
//! `[projects]` aliases still exist and win over the roots; the precedence lives
//! in `crate::sessions::resolver`, and this module only owns the configured
//! roots and the first-segment extraction under them.
//!
//! The roots live in the machine-local config, and the identity is a directory
//! name the operator chose when cloning, so no machine path reaches report
//! identity through this path. An unmatched directory is still unknown.

use std::path::{Component, Path, PathBuf};

/// The configured checkout-layout roots: where repositories and worktrees live
/// on this machine, plus the repository names that resolve to unknown on
/// purpose. Both roots are optional; with neither set, resolution is exactly
/// today's exact-match aliases.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LayoutRoots {
    /// Every immediate child directory is a repository named after it.
    pub repositories: Option<PathBuf>,
    /// `<dir>/<repo>/<anything>` resolves to `<repo>`.
    pub worktrees: Option<PathBuf>,
    /// Repository names that resolve to the unknown bucket on purpose.
    pub ignore: Vec<String>,
}

impl LayoutRoots {
    /// True when neither root is set and nothing is ignored, which is exactly
    /// today's exact-match behaviour in the resolver.
    pub fn is_empty(&self) -> bool {
        self.repositories.is_none() && self.worktrees.is_none() && self.ignore.is_empty()
    }

    /// True when at least one of the two roots is set. With neither, the
    /// resolver keeps today's exact-match aliases, ignore list or not.
    pub fn has_root(&self) -> bool {
        self.repositories.is_some() || self.worktrees.is_some()
    }

    /// True when `working_dir` is exactly one of the configured roots.
    fn is_configured_root(&self, working_dir: &str) -> bool {
        let dir = Path::new(working_dir);
        [&self.repositories, &self.worktrees]
            .into_iter()
            .flatten()
            .any(|root| root.as_path() == dir)
    }

    /// True when `name` is deliberately sent to the unknown bucket.
    pub fn is_ignored(&self, name: &str) -> bool {
        self.ignore.iter().any(|ignored| ignored == name)
    }
}

/// The first path segment after `root`, when `working_dir` sits strictly below
/// it: `<repositories>/<repo>/sub/dir` yields `repo`, and
/// `<worktrees>/<repo>/<task>/sub` yields `repo`. A directory equal to the root
/// itself yields nothing and resolves to unknown.
fn first_segment_after_root(root: &Path, working_dir: &str) -> Option<String> {
    if working_dir.is_empty() {
        return None;
    }
    let relative = Path::new(working_dir).strip_prefix(root).ok()?;
    match relative.components().next() {
        Some(Component::Normal(segment)) => {
            let name = segment.to_string_lossy().into_owned();
            if name.is_empty() { None } else { Some(name) }
        }
        _ => None,
    }
}

/// The repository name the layout roots imply for a working directory, or
/// nothing when neither root covers it. The worktree root is checked first
/// because it sits inside the repositories root on this machine, and checking
/// the repositories root first would name every worktree `.worktrees`.
pub fn layout_repository_name(layout: &LayoutRoots, working_dir: &str) -> Option<String> {
    // A directory equal to any configured root names no repository. Without
    // this, the worktrees root itself falls through to the repositories root
    // it sits inside and surfaces as `.worktrees`, a misconfiguration warning
    // for a correctly configured layout.
    if layout.is_configured_root(working_dir) {
        return None;
    }
    if let Some(worktrees) = &layout.worktrees
        && let Some(name) = first_segment_after_root(worktrees, working_dir)
    {
        return Some(name);
    }
    if let Some(repositories) = &layout.repositories
        && let Some(name) = first_segment_after_root(repositories, working_dir)
    {
        return Some(name);
    }
    None
}

/// Whether a layout-derived repository name is a valid report identity:
/// non-empty and never an absolute path, like an alias value
/// (`crate::config::aliases::AliasTable::new`), and never dot-named, so a
/// misconfigured root that would silently produce dot-named repositories is
/// refused at resolution instead.
pub fn valid_layout_repository_name(name: &str) -> bool {
    !name.is_empty() && !name.starts_with('/') && !name.starts_with('.')
}

/// The dot-named repository the roots imply for a working directory, when they
/// imply one: the misconfiguration witness the ingest summary reports once per
/// pass, so a root that silently produces dot-named repositories is visible
/// rather than silently mapping to unknown.
pub fn layout_rejected_repository_name(layout: &LayoutRoots, working_dir: &str) -> Option<String> {
    match layout_repository_name(layout, working_dir) {
        Some(name) if name.starts_with('.') => Some(name),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout() -> LayoutRoots {
        LayoutRoots {
            repositories: Some(PathBuf::from("/r")),
            worktrees: Some(PathBuf::from("/r/.worktrees")),
            ignore: vec!["scratch".to_string()],
        }
    }

    #[test]
    fn repositories_root_names_the_immediate_child() {
        assert_eq!(
            layout_repository_name(&layout(), "/r/aub"),
            Some("aub".to_string())
        );
        assert_eq!(
            layout_repository_name(&layout(), "/r/aub/src/meter"),
            Some("aub".to_string())
        );
    }

    #[test]
    fn worktrees_root_names_the_repo_before_the_task() {
        assert_eq!(
            layout_repository_name(&layout(), "/r/.worktrees/aub/bugfix-x"),
            Some("aub".to_string())
        );
        assert_eq!(
            layout_repository_name(&layout(), "/r/.worktrees/aub/bugfix-x/src"),
            Some("aub".to_string())
        );
    }

    #[test]
    fn worktrees_root_wins_over_the_repositories_root_it_sits_inside() {
        // Checked repositories-first, every worktree would resolve to
        // `.worktrees`; worktrees-first resolves to the repository instead.
        assert_eq!(
            layout_repository_name(&layout(), "/r/.worktrees/aub/bugfix-x"),
            Some("aub".to_string())
        );
    }

    #[test]
    fn a_directory_equal_to_a_root_itself_resolves_to_nothing() {
        assert_eq!(layout_repository_name(&layout(), "/r"), None);
        // `/r/.worktrees` equals the worktrees root: it names nothing, and in
        // particular does not fall through to the repositories root as a
        // `.worktrees` repository, so a correct layout raises no warning.
        assert_eq!(layout_repository_name(&layout(), "/r/.worktrees"), None);
        assert_eq!(layout_repository_name(&layout(), "/r/.worktrees/"), None);
        assert_eq!(
            layout_rejected_repository_name(&layout(), "/r/.worktrees"),
            None
        );
    }

    #[test]
    fn a_directory_under_neither_root_resolves_to_nothing() {
        assert_eq!(layout_repository_name(&layout(), "/tmp/build"), None);
        assert_eq!(layout_repository_name(&layout(), "/elsewhere/x"), None);
        assert_eq!(layout_repository_name(&layout(), ""), None);
    }

    #[test]
    fn a_similar_prefix_is_not_a_root() {
        // `/r2` merely starts with the same characters as `/r`; only a slash
        // boundary counts, so this is not under either root.
        assert_eq!(layout_repository_name(&layout(), "/r2/aub"), None);
    }

    #[test]
    fn dot_named_repositories_are_invalid_but_still_extracted() {
        // Without a worktrees root, the worktrees directory itself appears as
        // a repository name; extraction reports it so resolution can refuse it
        // visibly rather than silently producing a dot-named repository.
        let repos_only = LayoutRoots {
            repositories: Some(PathBuf::from("/r")),
            worktrees: None,
            ignore: Vec::new(),
        };
        assert_eq!(
            layout_repository_name(&repos_only, "/r/.worktrees/aub/x"),
            Some(".worktrees".to_string())
        );
        assert!(!valid_layout_repository_name(".worktrees"));
        assert_eq!(
            layout_rejected_repository_name(&repos_only, "/r/.worktrees/aub/x"),
            Some(".worktrees".to_string())
        );
        assert_eq!(layout_rejected_repository_name(&layout(), "/r/aub"), None);
    }

    #[test]
    fn ordinary_names_are_valid_and_ignored_names_match_exactly() {
        assert!(valid_layout_repository_name("aub"));
        assert!(!valid_layout_repository_name(""));
        assert!(!valid_layout_repository_name("/abs"));
        assert!(layout().is_ignored("scratch"));
        assert!(!layout().is_ignored("scratch-notes"));
    }

    #[test]
    fn an_empty_layout_is_empty() {
        assert!(LayoutRoots::default().is_empty());
        assert!(!layout().is_empty());
    }
}
