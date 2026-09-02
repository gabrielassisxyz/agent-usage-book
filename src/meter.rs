//! Sampling orchestration and provider adapters.
//!
//! May not depend on:
//! - SQLite directly (provider adapters do not know SQLite)
//! - presentation
//! - calibration

pub mod adapter;
pub mod anthropic;
#[cfg(test)]
pub mod synthetic;
pub mod transport;
