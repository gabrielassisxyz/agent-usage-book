//! The session- and run-keyed usage aggregation behind `aub export` (`aub-xus.7`,
//! PLAN.md 5, 27, 37).
//!
//! `aub export` joins the external friction ledger on a shared identifier: a session id
//! or a run id. This module owns the read side of that: it walks the stored sessions and
//! the canonical usage events joined to them, sums each session's usage by token class,
//! and rolls those up to whichever key the export is being produced for. The typed
//! record shape, the JSONL serialization and the privacy scan live outside the store
//! boundary; this module only produces the numbers and the identifiers they belong to.
//!
//! Two data-model facts shape the query. A `usage_event` carries only the source's own
//! session string, not the namespaced identity, so an event whose session string
//! matches sessions from two different sources is genuinely ambiguous; those events are
//! counted into an explicit unresolved bucket rather than attributed to a guess. And a
//! session without a run id contributes to session-keyed exports but is skipped by a
//! run-keyed one, because it has no run key to join on.
//!
//! May not depend on:
//! - HTTP or terminal-formatting crates
//! - presentation
//! - provider adapters

use std::collections::BTreeMap;

use rusqlite::Connection;

use crate::domain::ids::{NativeRunId, NativeSessionId, SourceNamespace};
use crate::domain::time::UtcTimestamp;
use crate::error::Error;
use crate::store::ledger_generation;

/// Which shared identifier an export is keyed on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportKey {
    /// One record per stored session, keyed by its namespaced session identity.
    Session,
    /// One record per run identifier, aggregating every session that carries it.
    Run,
}

impl ExportKey {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Session => "session-id",
            Self::Run => "run-id",
        }
    }
}

/// Per-token-class usage counts, kept in a sorted map so a re-run over unchanged data
/// serializes byte-identically.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageByTokenClass(BTreeMap<String, i64>);

impl UsageByTokenClass {
    fn add(&mut self, token_class: &str, count: i64) {
        *self.0.entry(token_class.to_string()).or_insert(0) += count;
    }

    /// The counts, token class ascending.
    pub fn entries(&self) -> impl Iterator<Item = (&str, i64)> {
        self.0.iter().map(|(k, v)| (k.as_str(), *v))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn total(&self) -> i64 {
        self.0.values().sum()
    }
}

/// One exported row: an identifier, the sessions it covers, its usage and the logical
/// identifiers the caller chose to include.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportRow {
    /// The key value: a namespaced `source:native` session id, or a bare run id.
    pub key: String,
    /// How many stored sessions this row aggregates (always 1 for a session-keyed row).
    pub session_count: u32,
    /// The earliest session start and latest session end across the covered sessions.
    pub first_start: UtcTimestamp,
    pub last_end: Option<UtcTimestamp>,
    /// Usage summed by token class.
    pub usage: UsageByTokenClass,
    /// Present only when `include_logical_ids` was set: the logical project and
    /// repository keys, deduplicated and sorted. A run spanning two projects lists both.
    pub project_keys: Vec<String>,
    pub repository_keys: Vec<String>,
}

/// A whole export: the rows plus the generations they were produced from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportData {
    pub key: ExportKey,
    pub included_logical_ids: bool,
    pub ledger_generation: u64,
    /// Rows a source-ambiguous session string contributed to, which could not be
    /// attributed to one namespaced session. Zero in the common case.
    pub unresolved_events: u64,
    pub rows: Vec<ExportRow>,
}

/// One stored session with the usage that joined to it.
struct SessionUsage {
    source: SourceNamespace,
    native: NativeSessionId,
    run: Option<NativeRunId>,
    project_key: String,
    repository_key: String,
    start: UtcTimestamp,
    end: Option<UtcTimestamp>,
    usage: UsageByTokenClass,
}

impl SessionUsage {
    fn namespaced_key(&self) -> String {
        format!("{}:{}", self.source.as_str(), self.native.as_str())
    }
}

/// Reads every stored session and the canonical usage that joins to it, then rolls the
/// per-session totals up to `key`.
pub fn assemble_export(
    conn: &Connection,
    key: ExportKey,
    include_logical_ids: bool,
) -> Result<ExportData, Error> {
    let ledger_generation = ledger_generation::current(conn)?.value();
    let (sessions, unresolved_events) = session_usage(conn)?;

    let rows = match key {
        ExportKey::Session => session_rows(sessions, include_logical_ids),
        ExportKey::Run => run_rows(sessions, include_logical_ids),
    };

    Ok(ExportData {
        key,
        included_logical_ids: include_logical_ids,
        ledger_generation,
        unresolved_events,
        rows,
    })
}

/// Every session with its joined usage, plus the count of usage events whose session
/// string matched more than one namespaced session and so could not be attributed.
fn session_usage(conn: &Connection) -> Result<(Vec<SessionUsage>, u64), Error> {
    let mut sessions: Vec<SessionUsage> = {
        let mut stmt = conn
            .prepare(
                "SELECT id, source, native_session_id, run_id, project_key, repository_key, \
                        start, end
                 FROM session ORDER BY source, native_session_id",
            )
            .map_err(|e| Error::Store(format!("cannot prepare the session scan: {e}")))?;
        let rows = stmt
            .query_map([], |row| {
                Ok(SessionUsage {
                    source: SourceNamespace::new(row.get::<_, String>(1)?),
                    native: NativeSessionId::new(row.get::<_, String>(2)?),
                    run: row.get::<_, Option<String>>(3)?.map(NativeRunId::new),
                    project_key: row.get::<_, String>(4)?,
                    repository_key: row.get::<_, String>(5)?,
                    start: UtcTimestamp::from_unix_nanos(row.get::<_, i64>(6)?),
                    end: row
                        .get::<_, Option<i64>>(7)?
                        .map(UtcTimestamp::from_unix_nanos),
                    usage: UsageByTokenClass::default(),
                })
            })
            .map_err(|e| Error::Store(format!("cannot scan sessions: {e}")))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| Error::Store(format!("cannot read a session row: {e}")))?);
        }
        out
    };

    // How many namespaced sessions share each native session string. A string owned by
    // exactly one session attributes cleanly; one shared by several is unresolved.
    let mut owners: BTreeMap<String, usize> = BTreeMap::new();
    for session in &sessions {
        *owners
            .entry(session.native.as_str().to_string())
            .or_insert(0) += 1;
    }

    let mut unresolved_events = 0u64;
    let mut stmt = conn
        .prepare(
            "SELECT e.session_id, c.token_class, SUM(c.count)
             FROM usage_event e
             JOIN usage_component c ON c.event_id = e.id
             WHERE e.session_id IS NOT NULL
             GROUP BY e.session_id, c.token_class",
        )
        .map_err(|e| Error::Store(format!("cannot prepare the usage join: {e}")))?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })
        .map_err(|e| Error::Store(format!("cannot read joined usage: {e}")))?;

    for row in rows {
        let (native, token_class, count) =
            row.map_err(|e| Error::Store(format!("cannot read a usage row: {e}")))?;
        match owners.get(&native).copied().unwrap_or(0) {
            1 => {
                let session = sessions
                    .iter_mut()
                    .find(|s| s.native.as_str() == native)
                    .expect("owners counted exactly one session for this string");
                session.usage.add(&token_class, count);
            }
            0 => unresolved_events += 1,
            _ => unresolved_events += 1,
        }
    }

    Ok((sessions, unresolved_events))
}

fn session_rows(sessions: Vec<SessionUsage>, include_logical_ids: bool) -> Vec<ExportRow> {
    sessions
        .into_iter()
        .map(|s| {
            let (project_keys, repository_keys) = if include_logical_ids {
                (vec![s.project_key.clone()], vec![s.repository_key.clone()])
            } else {
                (Vec::new(), Vec::new())
            };
            ExportRow {
                key: s.namespaced_key(),
                session_count: 1,
                first_start: s.start,
                last_end: s.end,
                usage: s.usage,
                project_keys,
                repository_keys,
            }
        })
        .collect()
}

fn run_rows(sessions: Vec<SessionUsage>, include_logical_ids: bool) -> Vec<ExportRow> {
    let mut by_run: BTreeMap<String, ExportRow> = BTreeMap::new();
    for session in sessions {
        let Some(run) = session.run.as_ref() else {
            continue;
        };
        let row = by_run
            .entry(run.as_str().to_string())
            .or_insert_with(|| ExportRow {
                key: run.as_str().to_string(),
                session_count: 0,
                first_start: session.start,
                last_end: session.end,
                usage: UsageByTokenClass::default(),
                project_keys: Vec::new(),
                repository_keys: Vec::new(),
            });
        row.session_count += 1;
        row.first_start = row.first_start.min(session.start);
        row.last_end = match (row.last_end, session.end) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (Some(a), None) | (None, Some(a)) => Some(a),
            (None, None) => None,
        };
        for (token_class, count) in session.usage.entries() {
            row.usage.add(token_class, count);
        }
        if include_logical_ids {
            push_sorted_unique(&mut row.project_keys, session.project_key);
            push_sorted_unique(&mut row.repository_keys, session.repository_key);
        }
    }
    by_run.into_values().collect()
}

fn push_sorted_unique(target: &mut Vec<String>, value: String) {
    if let Err(index) = target.binary_search(&value) {
        target.insert(index, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::time::{FakeClock, MonotonicDuration};
    use crate::store::connection::{AccessMode, PragmaPolicy, open};
    use crate::store::migrate::run_migrations;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new() -> Self {
            let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "aub-store-export-test-{}-{suffix}",
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

    fn conn() -> (ScratchDir, Connection) {
        let scratch = ScratchDir::new();
        let policy = PragmaPolicy {
            busy_timeout: MonotonicDuration::from_millis(1000),
        };
        let mut conn = open(
            &scratch.path().join("export.db"),
            AccessMode::ReadWrite,
            &policy,
        )
        .unwrap();
        run_migrations(
            &mut conn,
            &crate::store::migrations::registry(),
            None,
            &FakeClock::new(UtcTimestamp::from_unix_nanos(0)),
        )
        .unwrap();
        (scratch, conn)
    }

    #[allow(clippy::too_many_arguments)]
    fn seed_session(
        conn: &Connection,
        source: &str,
        native: &str,
        run: Option<&str>,
        project: &str,
        repository: &str,
        start: i64,
        end: Option<i64>,
    ) {
        conn.execute(
            "INSERT INTO session (source, native_session_id, run_id, project_key, repository_key, start, end)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![source, native, run, project, repository, start, end],
        )
        .expect("session insert");
    }

    fn seed_usage(conn: &Connection, session_native: &str, token_class: &str, count: i64) {
        let event_id: i64 = conn
            .query_row(
                "INSERT INTO usage_event (
                    canonical_event_id, session_id, evidence_kind, source_provenance,
                    parser_version, created_at
                 ) VALUES (?1, ?2, 'transcript', 'claude-code', 'v1', 100) RETURNING id",
                rusqlite::params![
                    format!("ce-{}-{}-{}", session_native, token_class, count),
                    session_native
                ],
                |row| row.get::<_, i64>(0),
            )
            .expect("usage_event insert");
        conn.execute(
            "INSERT INTO usage_component (event_id, token_class, count) VALUES (?1, ?2, ?3)",
            rusqlite::params![event_id, token_class, count],
        )
        .expect("usage_component insert");
    }

    #[test]
    fn session_keyed_export_sums_usage_per_token_class() {
        let (_s, conn) = conn();
        seed_session(
            &conn,
            "claude-code",
            "sess-a",
            Some("run-1"),
            "proj-x",
            "repo-x",
            10,
            Some(20),
        );
        seed_usage(&conn, "sess-a", "input", 100);
        seed_usage(&conn, "sess-a", "output", 40);
        seed_usage(&conn, "sess-a", "input", 5);

        let data = assemble_export(&conn, ExportKey::Session, false).unwrap();
        assert_eq!(data.key, ExportKey::Session);
        assert_eq!(data.rows.len(), 1);
        let row = &data.rows[0];
        assert_eq!(row.key, "claude-code:sess-a");
        assert_eq!(row.session_count, 1);
        assert_eq!(row.usage.total(), 145);
        let entries: Vec<_> = row.usage.entries().collect();
        assert_eq!(entries, vec![("input", 105), ("output", 40)]);
        assert!(
            row.project_keys.is_empty(),
            "logical ids excluded by default"
        );
    }

    #[test]
    fn run_keyed_export_aggregates_every_session_carrying_the_run() {
        let (_s, conn) = conn();
        seed_session(
            &conn,
            "claude-code",
            "sess-a",
            Some("run-1"),
            "proj-x",
            "repo-x",
            10,
            Some(20),
        );
        seed_session(
            &conn,
            "codex",
            "sess-b",
            Some("run-1"),
            "proj-y",
            "repo-y",
            5,
            Some(30),
        );
        seed_session(
            &conn,
            "claude-code",
            "sess-c",
            None,
            "proj-x",
            "repo-x",
            1,
            Some(2),
        );
        seed_usage(&conn, "sess-a", "input", 100);
        seed_usage(&conn, "sess-b", "input", 200);
        seed_usage(&conn, "sess-c", "input", 999);

        let data = assemble_export(&conn, ExportKey::Run, true).unwrap();
        assert_eq!(
            data.rows.len(),
            1,
            "a run-less session contributes no run row"
        );
        let row = &data.rows[0];
        assert_eq!(row.key, "run-1");
        assert_eq!(row.session_count, 2);
        assert_eq!(row.first_start, UtcTimestamp::from_unix_nanos(5));
        assert_eq!(row.last_end, Some(UtcTimestamp::from_unix_nanos(30)));
        assert_eq!(row.usage.total(), 300);
        assert_eq!(row.project_keys, vec!["proj-x", "proj-y"]);
        assert_eq!(row.repository_keys, vec!["repo-x", "repo-y"]);
    }

    #[test]
    fn logical_ids_appear_only_when_requested() {
        let (_s, conn) = conn();
        seed_session(
            &conn,
            "claude-code",
            "sess-a",
            None,
            "proj-x",
            "repo-x",
            10,
            Some(20),
        );
        seed_usage(&conn, "sess-a", "input", 1);

        let without = assemble_export(&conn, ExportKey::Session, false).unwrap();
        assert!(without.rows[0].project_keys.is_empty());
        assert!(without.rows[0].repository_keys.is_empty());

        let with = assemble_export(&conn, ExportKey::Session, true).unwrap();
        assert_eq!(with.rows[0].project_keys, vec!["proj-x"]);
        assert_eq!(with.rows[0].repository_keys, vec!["repo-x"]);
    }

    /// A native session string owned by two namespaced sessions is unresolved: its
    /// usage is counted into the explicit bucket, never attributed to a guess. The
    /// near-identical resolved case (a unique string) attributes cleanly.
    #[test]
    fn a_source_ambiguous_session_string_is_unresolved_not_guessed() {
        let (_s, conn) = conn();
        seed_session(
            &conn,
            "claude-code",
            "shared",
            None,
            "proj-x",
            "repo-x",
            1,
            Some(2),
        );
        seed_session(
            &conn,
            "codex",
            "shared",
            None,
            "proj-y",
            "repo-y",
            1,
            Some(2),
        );
        seed_session(
            &conn,
            "claude-code",
            "unique",
            None,
            "proj-z",
            "repo-z",
            1,
            Some(2),
        );
        seed_usage(&conn, "shared", "input", 100);
        seed_usage(&conn, "shared", "output", 50);
        seed_usage(&conn, "unique", "input", 7);

        let data = assemble_export(&conn, ExportKey::Session, false).unwrap();
        assert_eq!(
            data.unresolved_events, 2,
            "both 'shared' rows are unattributable"
        );
        let resolved: Vec<_> = data.rows.iter().filter(|r| !r.usage.is_empty()).collect();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].key, "claude-code:unique");
        assert_eq!(resolved[0].usage.total(), 7);
    }

    /// Re-running the assembly over unchanged data produces an identical structure,
    /// including row and token-class order.
    #[test]
    fn re_running_over_unchanged_data_is_deterministic() {
        let (_s, conn) = conn();
        seed_session(
            &conn,
            "claude-code",
            "sess-b",
            Some("run-2"),
            "proj-x",
            "repo-x",
            1,
            Some(2),
        );
        seed_session(
            &conn,
            "claude-code",
            "sess-a",
            Some("run-1"),
            "proj-x",
            "repo-x",
            3,
            Some(4),
        );
        seed_usage(&conn, "sess-a", "output", 2);
        seed_usage(&conn, "sess-a", "input", 1);
        seed_usage(&conn, "sess-b", "input", 9);

        let first = assemble_export(&conn, ExportKey::Session, true).unwrap();
        let second = assemble_export(&conn, ExportKey::Session, true).unwrap();
        assert_eq!(first, second);
        assert_eq!(
            first
                .rows
                .iter()
                .map(|r| r.key.as_str())
                .collect::<Vec<_>>(),
            vec!["claude-code:sess-a", "claude-code:sess-b"],
            "rows come out in a stable order"
        );
    }
}
