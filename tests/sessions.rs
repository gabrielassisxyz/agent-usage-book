//! Integration tests for session timelines and alias resolution (`aub-lqe.12`,
//! PLAN.md 12.8, 19.1, 19.3).
//!
//! May not depend on:
//! - presentation
//! - provider adapters
//! - HTTP or terminal formatting

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use agent_usage_book::config::AliasTable;
use agent_usage_book::domain::ids::{NativeSessionId, SessionId, SourceNamespace};
use agent_usage_book::domain::time::{FakeClock, MonotonicDuration, UtcTimestamp};
use agent_usage_book::sessions::resolver::{
    ProjectKey, UNKNOWN_PROJECT, UNKNOWN_REPOSITORY, resolve_project, resolve_repository,
};
use agent_usage_book::sessions::timeline::{
    count_events_by_project, derive_session_bounds, rebuild_sessions,
};
use agent_usage_book::store::connection::{AccessMode, PragmaPolicy, open};
use agent_usage_book::store::session::load_all_sessions;

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new() -> Self {
        let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "aub-sessions-integration-test-{}-{suffix}",
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
    let db_path = scratch.path().join("session_integration.db");
    let policy = PragmaPolicy {
        busy_timeout: MonotonicDuration::from_millis(1000),
    };
    let mut conn = open(&db_path, AccessMode::ReadWrite, &policy).unwrap();
    agent_usage_book::store::migrate::run_migrations(
        &mut conn,
        &agent_usage_book::store::migrations::registry(),
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

#[test]
fn two_identical_native_session_ids_from_different_sources_remain_distinct() {
    let (_scratch, mut conn) = fixture_conn();
    let events = vec![
        (session("claude-code", "session-alpha"), ts(10_000)),
        (session("codex", "session-alpha"), ts(20_000)),
        (session("pi", "session-alpha"), ts(30_000)),
    ];
    let dirs = HashMap::new();
    let projects = aliases(&[]);
    let repositories = aliases(&[]);

    let count = rebuild_sessions(&mut conn, &events, &dirs, &projects, &repositories).unwrap();
    assert_eq!(count, 3, "three distinct source namespaces");

    let stored = load_all_sessions(&conn).unwrap();
    assert_eq!(stored.len(), 3);
    assert_eq!(stored[0].source().as_str(), "claude-code");
    assert_eq!(stored[1].source().as_str(), "codex");
    assert_eq!(stored[2].source().as_str(), "pi");

    for s in &stored {
        assert_eq!(s.native_session_id().as_str(), "session-alpha");
    }
}

#[test]
fn alias_resolution_for_project_and_repository_with_unknown_buckets() {
    let table = aliases(&[
        ("/home/dev/work/aub", "agent-usage-book"),
        ("/home/dev/work/other", "other-tool"),
    ]);

    assert_eq!(
        resolve_project(&table, Some("/home/dev/work/aub")).as_str(),
        "agent-usage-book"
    );
    assert_eq!(
        resolve_repository(&table, Some("/home/dev/work/aub")).as_str(),
        "agent-usage-book"
    );
    assert_eq!(
        resolve_project(&table, Some("/home/dev/work/unmapped")).as_str(),
        UNKNOWN_PROJECT
    );
    assert_eq!(
        resolve_repository(&table, Some("/home/dev/work/unmapped")).as_str(),
        UNKNOWN_REPOSITORY
    );
    assert_eq!(resolve_project(&table, None).as_str(), UNKNOWN_PROJECT);
    assert_eq!(
        resolve_repository(&table, None).as_str(),
        UNKNOWN_REPOSITORY
    );
}

#[test]
fn no_absolute_machine_path_stored_as_report_identity() {
    let (_scratch, mut conn) = fixture_conn();
    let mapped = session("claude-code", "sess-mapped");
    let unmapped = session("claude-code", "sess-unmapped");
    let events = vec![(mapped.clone(), ts(1000)), (unmapped.clone(), ts(2000))];
    let dirs = HashMap::from([
        (mapped.clone(), Some("/home/developer/code/aub".to_string())),
        (unmapped.clone(), Some("/var/tmp/scratch".to_string())),
    ]);
    let projects = aliases(&[("/home/developer/code/aub", "agent-usage-book")]);
    let repositories = aliases(&[("/home/developer/code/aub", "agent-usage-book")]);

    rebuild_sessions(&mut conn, &events, &dirs, &projects, &repositories).unwrap();
    let stored = load_all_sessions(&conn).unwrap();

    for row in &stored {
        assert!(
            !row.project_key().as_str().starts_with('/'),
            "project key must not be a machine path: {}",
            row.project_key().as_str()
        );
        assert!(
            !row.repository_key().as_str().starts_with('/'),
            "repository key must not be a machine path: {}",
            row.repository_key().as_str()
        );
    }
}

#[test]
fn report_grouped_by_project_accounts_for_every_canonical_event_with_unknown_bucket_visible() {
    let s1 = session("claude-code", "s1");
    let s2 = session("claude-code", "s2");
    let s3 = session("codex", "s3");
    let events = vec![
        (s1.clone(), ts(100)),
        (s1.clone(), ts(200)),
        (s2.clone(), ts(300)),
        (s3.clone(), ts(400)),
    ];
    let dirs = HashMap::from([
        (s1.clone(), Some("/work/aub".to_string())),
        (s2.clone(), Some("/work/other".to_string())),
        (s3.clone(), None),
    ]);
    let projects = aliases(&[("/work/aub", "agent-usage-book")]);

    let counts = count_events_by_project(&events, &dirs, &projects);
    let sum: usize = counts.values().sum();
    assert_eq!(sum, 4, "every event must be accounted for");
    assert_eq!(
        counts.get(&ProjectKey::new("agent-usage-book")),
        Some(&2),
        "s1 has 2 events mapped to agent-usage-book"
    );
    assert_eq!(
        counts.get(&ProjectKey::new(UNKNOWN_PROJECT)),
        Some(&2),
        "s2 (/work/other) and s3 (None) land in unknown-project"
    );
}

#[test]
fn session_start_and_end_derive_from_event_timestamps() {
    let timestamps = vec![ts(5000), ts(1000), ts(3000), ts(7000)];
    let (start, end) = derive_session_bounds(&timestamps).unwrap();
    assert_eq!(start, ts(1000));
    assert_eq!(end, Some(ts(7000)));

    let single = vec![ts(42)];
    let (s_start, s_end) = derive_session_bounds(&single).unwrap();
    assert_eq!(s_start, ts(42));
    assert_eq!(s_end, Some(ts(42)));

    assert_eq!(derive_session_bounds(&[]), None);
}

#[test]
fn rebuild_determinism_property_over_sessions() {
    let (_scratch, mut conn) = fixture_conn();
    let events = vec![
        (session("claude-code", "a"), ts(100)),
        (session("claude-code", "a"), ts(500)),
        (session("codex", "b"), ts(200)),
        (session("pi", "c"), ts(300)),
    ];
    let dirs = HashMap::from([
        (session("claude-code", "a"), Some("/aub".to_string())),
        (session("codex", "b"), Some("/other".to_string())),
    ]);
    let projects = aliases(&[("/aub", "proj-aub"), ("/other", "proj-other")]);
    let repos = aliases(&[("/aub", "repo-aub"), ("/other", "repo-other")]);

    rebuild_sessions(&mut conn, &events, &dirs, &projects, &repos).unwrap();
    let first = load_all_sessions(&conn).unwrap();

    // Rebuild again on fresh table
    agent_usage_book::store::session::clear_all_sessions(&conn).unwrap();
    rebuild_sessions(&mut conn, &events, &dirs, &projects, &repos).unwrap();
    let second = load_all_sessions(&conn).unwrap();

    assert_eq!(first, second, "rebuilt sessions must be strictly identical");
    assert_eq!(first.len(), 3);
}
