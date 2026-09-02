//! Session timeline building and the rebuild path (`aub-lqe.12`, PLAN.md 12.8,
//! 19.1, 19.3).
//!
//! The session is the join that makes everything else possible, so it is normalized
//! immediately and namespaced by its source. Timelines are derived from usage-event
//! timestamps where the source does not state them, with the derivation documented
//! on [`derive_session_bounds`]; sessions are rebuildable from usage events, and
//! [`rebuild_sessions`] is that path, resolving project and repository through the
//! configured aliases so the stored rows carry logical identities, never machine
//! paths.

use std::collections::{BTreeMap, HashMap};

use crate::config::AliasTable;
use crate::domain::ids::{NativeSessionId, SessionId, SourceNamespace};
use crate::domain::time::UtcTimestamp;
use crate::error::Error;
use crate::sessions::resolver::{resolve_project, resolve_repository};
use crate::store::session::{NewSession, replace_all_sessions};

/// A normalized session timeline: the namespaced session and its derived bounds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionTimeline {
    pub session: SessionId,
    pub start: UtcTimestamp,
    pub end: Option<UtcTimestamp>,
}

/// Derives a session's bounds from its event timestamps.
///
/// The documented derivation: start is the earliest event timestamp and end is the
/// latest, so a single event yields `start == end`. `end` is `None` only for an
/// empty event list, which is not a session at all; the function refuses that case
/// rather than inventing a bound for it.
pub fn derive_session_bounds(events: &[UtcTimestamp]) -> (UtcTimestamp, Option<UtcTimestamp>) {
    let start = *events
        .iter()
        .min()
        .expect("a session timeline needs at least one event");
    let end = *events
        .iter()
        .max()
        .expect("a session timeline needs at least one event");
    (start, Some(end))
}

/// Groups events by session and derives each session's bounds, in (source, native
/// session id) order.
///
/// Grouping keys on the namespaced pair as strings rather than on [`SessionId`]
/// itself, which carries no `Ord`; the reconstructed identifiers are the same
/// values the events carried.
pub fn build_timelines(events: &[(SessionId, UtcTimestamp)]) -> Vec<SessionTimeline> {
    let mut by_session: BTreeMap<(String, String), Vec<UtcTimestamp>> = BTreeMap::new();
    for (session, at) in events {
        by_session
            .entry((
                session.source().as_str().to_string(),
                session.native().as_str().to_string(),
            ))
            .or_default()
            .push(*at);
    }
    by_session
        .into_iter()
        .map(|((source, native), times)| {
            let (start, end) = derive_session_bounds(&times);
            SessionTimeline {
                session: SessionId::new(SourceNamespace::new(source), NativeSessionId::new(native)),
                start,
                end,
            }
        })
        .collect()
}

/// Counts events per resolved project: the resolution-level guarantee a report
/// grouped by project depends on. Every event lands in exactly one bucket, and the
/// unknown bucket is visible when a working directory is unmapped or absent.
pub fn count_events_by_project(
    events: &[(SessionId, UtcTimestamp)],
    working_dirs: &HashMap<SessionId, Option<String>>,
    project_aliases: &AliasTable,
) -> BTreeMap<crate::sessions::resolver::ProjectKey, usize> {
    let mut counts: BTreeMap<crate::sessions::resolver::ProjectKey, usize> = BTreeMap::new();
    for (session, _) in events {
        let working_dir = working_dirs.get(session).and_then(|dir| dir.as_deref());
        let key = resolve_project(project_aliases, working_dir);
        *counts.entry(key).or_insert(0) += 1;
    }
    counts
}

/// The rebuild path: replaces every stored session with rows derived from the given
/// events, resolving project and repository through the configured aliases.
///
/// Run identifiers are not derivable from usage events, so rebuilt rows carry none;
/// a run id on a session row is source-stated evidence, not a derived fact. Returns
/// the number of sessions written.
pub fn rebuild_sessions(
    conn: &mut rusqlite::Connection,
    events: &[(SessionId, UtcTimestamp)],
    working_dirs: &HashMap<SessionId, Option<String>>,
    project_aliases: &AliasTable,
    repository_aliases: &AliasTable,
) -> Result<usize, Error> {
    let timelines = build_timelines(events);
    let sessions: Vec<NewSession> = timelines
        .iter()
        .map(|timeline| {
            let working_dir = working_dirs
                .get(&timeline.session)
                .and_then(|dir| dir.as_deref());
            NewSession {
                source: timeline.session.source().clone(),
                native_session_id: NativeSessionId::new(timeline.session.native().as_str()),
                start: timeline.start,
                end: timeline.end,
                project_key: resolve_project(project_aliases, working_dir),
                repository_key: resolve_repository(repository_aliases, working_dir),
                run_id: None,
            }
        })
        .collect();
    replace_all_sessions(conn, &sessions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ids::{NativeSessionId, SourceNamespace};
    use crate::domain::time::{FakeClock, MonotonicDuration};
    use crate::sessions::resolver::{UNKNOWN_PROJECT, UNKNOWN_REPOSITORY};
    use crate::store::connection::{AccessMode, PragmaPolicy, open};
    use crate::store::session::load_all_sessions;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new() -> Self {
            let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "aub-store-session-test-{}-{suffix}",
                std::process::id()
            ));
            std::fs::create_dir(&path).expect("scratch dir must be creatable");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn fixture_conn() -> (ScratchDir, rusqlite::Connection) {
        let scratch = ScratchDir::new();
        let db_path = scratch.path().join("session.db");
        let policy = PragmaPolicy {
            busy_timeout: MonotonicDuration::from_millis(1000),
        };
        let mut conn = open(&db_path, AccessMode::ReadWrite, &policy).unwrap();
        crate::store::migrate::run_migrations(
            &mut conn,
            &crate::store::migrations::registry(),
            None,
            &FakeClock::new(UtcTimestamp::from_unix_nanos(0)),
        )
        .unwrap();
        (scratch, conn)
    }

    fn ts(nanos: i64) -> UtcTimestamp {
        UtcTimestamp::from_unix_nanos(nanos)
    }

    fn session(source: &str, native: &str) -> SessionId {
        SessionId::new(SourceNamespace::new(source), NativeSessionId::new(native))
    }

    fn aliases(pairs: &[(&str, &str)]) -> AliasTable {
        AliasTable::new(
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        )
        .unwrap()
    }

    /// The documented derivation: earliest event is the start, latest is the end,
    /// and a single event yields start == end.
    #[test]
    fn bounds_derive_from_event_timestamps() {
        let (start, end) = derive_session_bounds(&[ts(3_000), ts(1_000), ts(2_000)]);
        assert_eq!(start, ts(1_000));
        assert_eq!(end, Some(ts(3_000)));

        let (start, end) = derive_session_bounds(&[ts(5_000)]);
        assert_eq!(start, ts(5_000));
        assert_eq!(end, Some(ts(5_000)));
    }

    #[test]
    #[should_panic(expected = "at least one event")]
    fn empty_event_list_is_not_a_session() {
        let _ = derive_session_bounds(&[]);
    }

    /// Two textually identical native session identifiers from different sources
    /// are distinct sessions, and the table stores both.
    #[test]
    fn identical_native_ids_from_different_sources_are_distinct() {
        let (_scratch, mut conn) = fixture_conn();
        let events = vec![
            (session("claude-code", "sess-1"), ts(1_000)),
            (session("codex", "sess-1"), ts(2_000)),
        ];
        let dirs = HashMap::new();
        let projects = aliases(&[]);
        let repositories = aliases(&[]);
        rebuild_sessions(&mut conn, &events, &dirs, &projects, &repositories).unwrap();

        let stored = load_all_sessions(&conn).unwrap();
        assert_eq!(stored.len(), 2);
        assert_eq!(stored[0].source().as_str(), "claude-code");
        assert_eq!(stored[1].source().as_str(), "codex");
        assert_eq!(
            stored[0].native_session_id().as_str(),
            stored[1].native_session_id().as_str()
        );
        assert_ne!(stored[0].id(), stored[1].id());
    }

    /// No absolute machine path is stored as report identity: the session table
    /// carries the logical keys, and unmapped work lands in the unknown buckets.
    #[test]
    fn no_absolute_path_is_stored_as_report_identity() {
        let (_scratch, mut conn) = fixture_conn();
        let mapped = session("claude-code", "mapped");
        let unmapped = session("claude-code", "unmapped");
        let events = vec![(mapped.clone(), ts(1_000)), (unmapped.clone(), ts(2_000))];
        let dirs = HashMap::from([
            (mapped.clone(), Some("/home/u/work/aub".to_string())),
            (unmapped.clone(), Some("/home/u/work/elsewhere".to_string())),
        ]);
        let projects = aliases(&[("/home/u/work/aub", "agent-usage-book")]);
        let repositories = aliases(&[("/home/u/work/aub", "agent-usage-book")]);
        rebuild_sessions(&mut conn, &events, &dirs, &projects, &repositories).unwrap();

        let stored = load_all_sessions(&conn).unwrap();
        for row in &stored {
            assert!(
                !row.project_key().as_str().contains('/'),
                "stored project key is a path: {}",
                row.project_key().as_str()
            );
            assert!(
                !row.repository_key().as_str().contains('/'),
                "stored repository key is a path: {}",
                row.repository_key().as_str()
            );
        }
        let mapped_row = stored
            .iter()
            .find(|row| row.native_session_id().as_str() == "mapped")
            .unwrap();
        assert_eq!(mapped_row.project_key().as_str(), "agent-usage-book");
        let unmapped_row = stored
            .iter()
            .find(|row| row.native_session_id().as_str() == "unmapped")
            .unwrap();
        assert_eq!(unmapped_row.project_key().as_str(), UNKNOWN_PROJECT);
        assert_eq!(unmapped_row.repository_key().as_str(), UNKNOWN_REPOSITORY);
    }

    /// A report grouped by project accounts for every canonical event, with the
    /// unknown bucket visible: the counts sum to the event total.
    #[test]
    fn project_grouping_accounts_for_every_event_with_the_unknown_bucket_visible() {
        let mapped = session("claude-code", "mapped");
        let unmapped = session("claude-code", "unmapped");
        let events = vec![
            (mapped.clone(), ts(1_000)),
            (mapped.clone(), ts(2_000)),
            (unmapped.clone(), ts(3_000)),
        ];
        let dirs = HashMap::from([
            (mapped.clone(), Some("/home/u/work/aub".to_string())),
            (unmapped.clone(), None),
        ]);
        let projects = aliases(&[("/home/u/work/aub", "agent-usage-book")]);

        let counts = count_events_by_project(&events, &dirs, &projects);
        let total: usize = counts.values().sum();
        assert_eq!(total, events.len(), "every event must land in a bucket");
        assert_eq!(
            counts.get(&crate::sessions::resolver::ProjectKey::new(
                "agent-usage-book"
            )),
            Some(&2)
        );
        assert_eq!(
            counts.get(&crate::sessions::resolver::ProjectKey::new(UNKNOWN_PROJECT)),
            Some(&1)
        );
    }

    /// Sessions are rebuildable from usage events: deleting and rebuilding from the
    /// same events reproduces the identical rows.
    #[test]
    fn rebuild_is_deterministic() {
        let (_scratch, mut conn) = fixture_conn();
        let events = vec![
            (session("claude-code", "s1"), ts(1_000)),
            (session("claude-code", "s1"), ts(2_000)),
            (session("codex", "s2"), ts(3_000)),
        ];
        let dirs = HashMap::from([(session("claude-code", "s1"), Some("/w".to_string()))]);
        let projects = aliases(&[("/w", "proj")]);
        let repositories = aliases(&[("/w", "repo")]);

        rebuild_sessions(&mut conn, &events, &dirs, &projects, &repositories).unwrap();
        let first = load_all_sessions(&conn).unwrap();

        // Delete and rebuild from the same events: identical rows come back.
        crate::store::session::clear_all_sessions(&conn).unwrap();
        rebuild_sessions(&mut conn, &events, &dirs, &projects, &repositories).unwrap();
        let second = load_all_sessions(&conn).unwrap();

        assert_eq!(first, second);
        assert_eq!(first.len(), 2);
        assert_eq!(first[0].start(), ts(1_000));
        assert_eq!(first[0].end(), Some(ts(2_000)));
        assert_eq!(first[0].project_key().as_str(), "proj");
        assert_eq!(first[0].repository_key().as_str(), "repo");
    }
}
