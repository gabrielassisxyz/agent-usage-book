//! Build-time provenance: the crate version, source revision and toolchain version that
//! produced a binary.
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

/// The toolchain version the binary was built with, as pinned by `rust-toolchain.toml`.
///
/// Set by `build.rs` at compile time from the `channel` value in `rust-toolchain.toml`;
/// never derived at runtime, since a binary cannot see the toolchain that built it once
/// it is running elsewhere. `build.rs` falls back to the sentinel `"unknown"` when the
/// file cannot be read, but that sentinel can never justify a calibration record's
/// provenance claim, so the tests below bind the reported value to the on-disk pin and
/// reject the sentinel: a build whose toolchain version cannot be determined fails
/// `cargo test`, which is `bin/ci`'s gate, instead of shipping a provenance field nothing
/// can verify.
pub fn toolchain_version() -> &'static str {
    env!("AUB_TOOLCHAIN_VERSION")
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

    /// Reads the `channel` value out of `rust-toolchain.toml`'s `[toolchain]` table as
    /// plain text, so the comparison is against the pin as data. Comparing
    /// `toolchain_version()` to a second compile-time constant compares the same value
    /// to itself and cannot fail no matter what the pin says.
    fn channel_from_toolchain_file() -> String {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/rust-toolchain.toml");
        let contents = fs::read_to_string(path).expect("rust-toolchain.toml must be readable");
        contents
            .lines()
            .skip_while(|line| line.trim() != "[toolchain]")
            .skip(1)
            .take_while(|line| !line.trim_start().starts_with('['))
            .find_map(|line| {
                let (key, value) = line.split_once('=')?;
                (key.trim() == "channel").then(|| value.trim().trim_matches('"').to_string())
            })
            .expect("[toolchain] must declare a channel")
    }

    #[test]
    fn toolchain_version_matches_the_pinned_channel_on_disk() {
        assert_eq!(toolchain_version(), channel_from_toolchain_file());
    }

    #[test]
    fn toolchain_version_is_a_version_not_a_sentinel() {
        let version = toolchain_version();
        assert!(!version.is_empty(), "toolchain version must not be empty");
        assert_ne!(
            version, "unknown",
            "toolchain version must not be the sentinel"
        );
        assert!(
            version.chars().all(|c| c.is_ascii_digit() || c == '.'),
            "expected a version like 1.97.1, got {version:?}"
        );
    }
}
