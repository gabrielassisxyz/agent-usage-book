//! Normalized session timelines.
//!
//! May not depend on:
//! - presentation
//! - provider adapters
//!
//! The session is the join that makes everything else possible, so it is normalized
//! immediately and namespaced by its source (`aub-lqe.12`, PLAN.md 12.8, 19.1).
//! Timelines derive from usage-event timestamps where the source does not state
//! them, and project and repository resolve through configured aliases into typed
//! logical identities, with unmapped work landing in the unknown buckets.

pub mod resolver;
pub mod timeline;

pub use resolver::{
    ProjectKey, RepositoryKey, UNKNOWN_PROJECT, UNKNOWN_REPOSITORY, resolve_project,
    resolve_repository,
};
pub use timeline::{
    SessionTimeline, build_timelines, count_events_by_project, derive_session_bounds,
    rebuild_sessions,
};
