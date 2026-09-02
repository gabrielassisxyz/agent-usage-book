//! Quantities, identifiers, freshness, window semantics, intervals, and provenance.
//!
//! May not depend on:
//! - SQLite, HTTP, or terminal-formatting crates
//! - transcript locations
//! - any adapter, workflow, or presentation layer

pub mod attempt;
pub mod credits;
pub mod failure;
pub mod freshness;
pub mod ids;
pub mod interval;
pub mod money;
pub mod provenance;
pub mod quota;
pub mod rate_card;
pub mod render;
pub mod rows;
pub mod time;
pub mod tokens;
pub mod window;
