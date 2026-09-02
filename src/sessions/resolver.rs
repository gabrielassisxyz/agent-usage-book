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
}
