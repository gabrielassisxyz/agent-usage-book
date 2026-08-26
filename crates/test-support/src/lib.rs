//! Test-only support shared by every suite in the repository.
//!
//! One owner for the harness is what keeps a swarm of interchangeable panes from
//! writing five temp-directory helpers, five fake clocks and five fixture loaders
//! with different bugs. This crate is a dev-dependency of the main package, so cargo
//! never links it into the release binary; the manifest check in
//! `tests/test_support.rs` asserts that structure so the guarantee cannot be silently
//! weakened by moving the dependency.
//!
//! The two non-negotiables are determinism and loud failure. Every generator takes an
//! explicit [`Seed`], and a failing property test prints the seed that produced it,
//! because a failure nobody can reproduce is a failure nobody will fix. The binary
//! locator resolves from the target directory the test run was given and fails naming
//! the expected path rather than falling back to `PATH`, because a binary found on
//! `PATH` may be another pane's build.

pub mod binary;
pub mod clock;
pub mod evidence;
pub mod fixture;
pub mod log_events;
pub mod rng;
pub mod state_dir;

pub use binary::aub_binary_in;
pub use clock::{Clock, FakeClock, SystemClock};
pub use evidence::{
    Attempt, AttemptOutcome, Component, Marker, MarkerKind, Observation, ResponseEvidenceRow,
    UsageEvent, Window,
};
pub use fixture::{fixture_path, load_fixture};
pub use log_events::{LogEvent, assert_event};
pub use rng::{Rng, Seed, check_property};
pub use state_dir::StateDir;
