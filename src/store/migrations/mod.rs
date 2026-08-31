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
//!
//! This file is shared by every schema bead, so a bead adds its migration under
//! reservation: reserve this file first, read the highest version already
//! present, take the next one, append the module line and the registry entry,
//! then commit path-scoped and release.

use super::migrate::Migration;

// File names are the migration's own version number, which cannot start a Rust
// identifier, so each module is declared with an explicit path rather than a
// bare `mod` line.
#[path = "0001_account_sample_run_policy_snapshot.rs"]
mod migration_0001;

// Visible to the crate so the sampling lease repository's own tests can migrate
// a fixture database up to exactly this version.
#[path = "0002_sampling_lease.rs"]
pub(crate) mod migration_0002;

#[path = "0003_ledger_generation.rs"]
pub(crate) mod migration_0003;

#[path = "0004_session_account_marker.rs"]
pub(crate) mod migration_0004;

#[path = "0005_usage_occurrence.rs"]
pub(crate) mod migration_0005;

#[path = "0006_transcript_file.rs"]
pub(crate) mod migration_0006;

#[path = "0007_task_event.rs"]
pub(crate) mod migration_0007;

/// Every migration this binary knows, in version order.
///
/// The framework is exercised by its own tests with synthetic registries; this
/// is the registry production code reads.
pub fn registry() -> Vec<Migration> {
    vec![
        migration_0001::migration(),
        migration_0002::migration(),
        migration_0003::migration(),
        migration_0004::migration(),
        migration_0005::migration(),
        migration_0006::migration(),
        migration_0007::migration(),
    ]
}
