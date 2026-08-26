//! Build-time provenance: the crate version and source revision that produced a binary.
//!
//! Calibration results and schema migrations persist these so a fit can state which
//! compiler and which source revision produced it, closing the reproducibility gap a bare
//! result would otherwise leave open.

/// The crate version declared in `Cargo.toml`.
pub fn crate_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// The git revision the binary was built from.
///
/// Set by `build.rs` at compile time from `git rev-parse HEAD`; never re-derived at
/// runtime, since a binary cannot see the tree it was built from once it is running
/// elsewhere. `build.rs` falls back to the sentinel `"unknown"` when no git checkout is
/// present, but that sentinel can never justify a calibration record's or a migration's
/// provenance claim, so it is not treated as an acceptable answer here: this project only
/// builds from a git checkout, and `source_revision_is_a_full_lowercase_sha` below rejects
/// anything that is not a real 40-character hex sha, sentinel included. A git-less build
/// therefore fails `cargo test`, which is `bin/ci`'s gate, instead of shipping a
/// provenance field nothing can verify.
pub fn source_revision() -> &'static str {
    env!("AUB_GIT_REVISION")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Reads the version out of `Cargo.toml`'s `[package]` table as plain text, so the
    /// comparison is against the manifest as data. Comparing `crate_version()` to a second
    /// `env!("CARGO_PKG_VERSION")` expansion compares the same compile-time constant to
    /// itself and cannot fail no matter what the manifest says.
    fn version_from_manifest() -> String {
        let manifest_path = concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml");
        let manifest = fs::read_to_string(manifest_path).expect("Cargo.toml must be readable");
        manifest
            .lines()
            .skip_while(|line| line.trim() != "[package]")
            .skip(1)
            .take_while(|line| !line.trim_start().starts_with('['))
            .find_map(|line| {
                let (key, value) = line.split_once('=')?;
                (key.trim() == "version").then(|| value.trim().trim_matches('"').to_string())
            })
            .expect("[package] must declare a version")
    }

    #[test]
    fn crate_version_matches_the_manifest_on_disk() {
        assert_eq!(crate_version(), version_from_manifest());
    }

    #[test]
    fn source_revision_is_a_full_lowercase_sha() {
        let revision = source_revision();
        assert_eq!(
            revision.len(),
            40,
            "expected a 40-character git sha, got {revision:?}"
        );
        assert!(
            revision.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')),
            "expected lowercase hex, got {revision:?}"
        );
    }
}
