//! Credits to quota-window relationship.
//!
//! May not depend on:
//! - transcripts (the calibration layer never parses transcripts)
//! - presentation
//!
//! The Credits to PercentDelta conversion requires a typed `WindowCalibration` witness
//! and is owned by this module; no global conversion witness exists.

pub mod activation;
pub mod contamination;
pub mod fitter;
pub mod health;
pub mod multivariate;
pub mod settlement;
