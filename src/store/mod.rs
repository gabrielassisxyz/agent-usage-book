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
pub mod attempt_crash_hook;
pub mod attribution_segment;
pub mod backup;
pub mod calibration;
pub mod connection;
pub mod cost_model;
pub mod ingest_quarantine;
pub mod ledger_generation;
pub mod meter_attempt;
pub mod meter_evidence;
pub mod migrate;
pub mod migrations;
pub mod rate_card;
pub mod sample_run;
pub mod sampling_lease;
pub mod sampling_policy_snapshot;
pub mod schema_audit;
pub mod session;
pub mod session_account_marker;
pub mod spool;
pub mod startup;
pub mod task_event;
pub mod task_identity;
pub mod transcript_file;
pub mod usage_component;
pub mod usage_event;
pub mod usage_occurrence;
