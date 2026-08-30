//! SQLite schema, migrations, repositories, and transactions.
//!
//! May not depend on:
//! - HTTP or terminal-formatting crates
//! - presentation
//! - provider adapters
//!
//! This is the only module that knows SQLite.

pub mod account;
pub mod connection;
pub mod migrate;
pub mod migrations;
pub mod sample_run;
pub mod sampling_lease;
pub mod sampling_policy_snapshot;
pub mod startup;
