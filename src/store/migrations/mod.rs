//! The migration registry: every numbered schema step this binary knows, in
//! version order.
//!
//! The ordering rule (aub-sth.4): migrations are numbered consecutively from 1,
//! each later schema bead appends exactly one migration holding the next
//! version, and a version once applied is never edited (forward-only). The
//! framework validates the registry against this rule before applying anything;
//! see [`super::migrate::run_migrations`].
//!
//! A later schema bead contributes its migration here and nowhere else in the
//! store module: one file per migration in this directory, a `pub mod` line
//! below, and one entry in [`registry`]. The framework itself never changes for
//! a new schema.

use super::migrate::Migration;

/// Every migration this binary knows, in version order.
///
/// Empty until the first schema bead lands (`aub-sth.5` and its successors).
/// The framework is exercised by its own tests with synthetic registries; this
/// is the registry production code reads.
pub fn registry() -> Vec<Migration> {
    vec![]
}
