//! Build-time provenance: the crate version and source revision that produced a binary.
//!
//! Calibration results and schema migrations persist these so a fit can state which
//! compiler and which source revision produced it, closing the reproducibility gap a bare
//! result would otherwise leave open.

/// The crate version declared in `Cargo.toml`.
pub fn crate_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// The git revision the binary was built from, or `"unknown"` outside a git checkout.
///
/// Set by `build.rs` at compile time; never re-derived at runtime, since a binary cannot
/// see the tree it was built from once it is running elsewhere.
pub fn source_revision() -> &'static str {
    env!("AUB_GIT_REVISION")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crate_version_is_non_empty_and_matches_manifest() {
        assert_eq!(crate_version(), env!("CARGO_PKG_VERSION"));
        assert!(!crate_version().is_empty());
    }

    #[test]
    fn source_revision_is_non_empty() {
        assert!(!source_revision().is_empty());
    }
}
