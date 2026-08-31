//! SQLite schema, migrations, repositories, and transactions.
//!
//! May not depend on:
//! - HTTP or terminal-formatting crates
//! - presentation
//! - provider adapters
//!
//! This is the only module that knows SQLite; `bin/checks/boundary-rules/16-sql-only-in-store`
//! checks it mechanically.
//!
//! Repository conventions, binding on every table this module adds:
//! - A repository method takes and returns domain types, never a raw row or a bare primitive.
//! - An evidence table exposes no update path and no delete path.
//! - A write transaction stays short: no network I/O and no unrelated work happens inside it.

pub mod account;
pub mod connection;
pub mod ledger_generation;
pub mod migrate;
pub mod migrations;
pub mod sample_run;
pub mod sampling_lease;
pub mod sampling_policy_snapshot;
pub mod session_account_marker;
pub mod startup;
