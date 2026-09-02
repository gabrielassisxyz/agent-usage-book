//! The project and repository alias tables (`aub-lqe.12`, PLAN.md 19.3).
//!
//! A source provides a working directory; the alias table maps that directory to a
//! configured logical identity. The logical identity is what reports store and
//! group by, so the table's values are validated to be logical names rather than
//! machine paths: an absolute path as a value would leak the machine into report
//! identity, which is exactly what the no-compiled-identity rule forbids. The
//! config file is machine-local, so paths as keys are fine; the value travels, and
//! the value is what this module guards.
//!
//! Resolution is an exact match on the working directory. A prefix match is
//! deliberately not offered: a sibling checkout of the same repository would be
//! silently attributed to the wrong logical identity, and the honest answer for an
//! unmapped directory is the unknown bucket, not a guess.

use std::collections::BTreeMap;

use crate::error::Error;

/// A configured alias table: working-directory path to logical identity.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AliasTable {
    entries: BTreeMap<String, String>,
}

impl AliasTable {
    /// Builds a table from raw path-to-name pairs, rejecting an empty path key, an
    /// empty logical name, or a logical name that is an absolute path.
    pub fn new(entries: BTreeMap<String, String>) -> Result<Self, Error> {
        for (path, name) in &entries {
            if path.is_empty() {
                return Err(Error::Usage("alias table: empty path key".into()));
            }
            if name.is_empty() {
                return Err(Error::Usage(format!(
                    "alias table: empty logical name for {path:?}"
                )));
            }
            if name.starts_with('/') {
                return Err(Error::Usage(format!(
                    "alias table: logical name {name:?} for {path:?} is an absolute path; \
                     report identity must be a logical name, never a machine path"
                )));
            }
        }
        Ok(Self { entries })
    }

    /// The logical identity for a working directory, when one is configured.
    pub fn resolve(&self, working_dir: &str) -> Option<&str> {
        self.entries.get(working_dir).map(String::as_str)
    }

    /// Every configured (path, logical name) pair, in path order.
    pub fn entries(&self) -> impl Iterator<Item = (&str, &str)> {
        self.entries.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(pairs: &[(&str, &str)]) -> AliasTable {
        AliasTable::new(
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        )
        .unwrap()
    }

    #[test]
    fn exact_match_resolves_and_unmapped_is_none() {
        let aliases = table(&[("/home/u/work/aub", "agent-usage-book")]);
        assert_eq!(
            aliases.resolve("/home/u/work/aub"),
            Some("agent-usage-book")
        );
        assert_eq!(aliases.resolve("/home/u/work/other"), None);
        // A sibling checkout of the same tree is not silently attributed.
        assert_eq!(aliases.resolve("/home/u/work/aub-copy"), None);
    }

    #[test]
    fn empty_path_key_is_rejected() {
        assert!(AliasTable::new(BTreeMap::from([("".into(), "x".into())])).is_err());
    }

    #[test]
    fn empty_logical_name_is_rejected() {
        assert!(AliasTable::new(BTreeMap::from([("/p".into(), "".into())])).is_err());
    }

    #[test]
    fn absolute_path_as_logical_name_is_rejected() {
        let err = AliasTable::new(BTreeMap::from([("/p".into(), "/etc".into())])).unwrap_err();
        assert!(err.to_string().contains("absolute path"));
    }

    #[test]
    fn entries_are_in_path_order() {
        let aliases = table(&[("/b", "two"), ("/a", "one")]);
        let entries: Vec<(&str, &str)> = aliases.entries().collect();
        assert_eq!(entries, vec![("/a", "one"), ("/b", "two")]);
    }
}
