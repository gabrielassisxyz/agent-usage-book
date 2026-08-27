//! Recursive discovery and source-specific parsers.
//!
//! May not depend on:
//! - calibration
//! - cost models
//! - subscription window capacity, API pricing, task history, or meter percentages

pub mod discovery;

pub use discovery::{DiscoveryError, DiscoveryOptions, SourceDiscovery, discover};
