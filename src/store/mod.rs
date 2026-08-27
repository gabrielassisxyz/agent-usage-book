//! SQLite schema, migrations, repositories, and transactions.
//!
//! May not depend on:
//! - HTTP or terminal-formatting crates
//! - presentation
//! - provider adapters
//!
//! This is the only module that knows SQLite.

pub mod connection;
pub mod startup;
