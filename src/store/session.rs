//! The normalized `session` table and its repository (`aub-lqe.12`, PLAN.md 12.8,
//! 19.1, 19.3).
//!
//! One row per namespaced session: the `UNIQUE (source, native_session_id)`
//! constraint is the database half of the namespacing rule, so two textually
//! identical native identifiers from different CLIs are distinct rows here even if
//! application code stopped checking.
//!
//! The row carries no mandatory account column (account assignment belongs to the
//! marker timeline), and project and repository are stored as typed logical keys,
//! never as machine paths: the resolver in `crate::sessions` produces the keys, and
//! this module only ever persists them.
//!
//! Sessions are rebuildable from usage events, so this repository exposes the
//! explicit replace path (`replace_all_sessions`) and no immutability trigger guards
//! the table: a rebuild deletes and re-creates, and the determinism of that path is
//! what the rebuild test asserts.
//!
//! May not depend on:
//! - HTTP or terminal-formatting crates
//! - presentation
//! - provider adapters

use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::domain::ids::{NativeRunId, NativeSessionId, SourceNamespace};
use crate::domain::time::UtcTimestamp;
use crate::error::Error;
use crate::sessions::resolver::{ProjectKey, RepositoryKey};

/// A session row's SQLite rowid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionDbId(i64);

impl SessionDbId {
    pub const fn new(value: i64) -> Self {
        Self(value)
    }

    pub const fn value(self) -> i64 {
        self.0
    }
}

/// A stored normalized session row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    id: SessionDbId,
    source: SourceNamespace,
    native_session_id: NativeSessionId,
    start: UtcTimestamp,
    end: Option<UtcTimestamp>,
    project_key: ProjectKey,
    repository_key: RepositoryKey,
    run_id: Option<NativeRunId>,
}

impl Session {
    pub fn id(&self) -> SessionDbId {
        self.id
    }

    pub fn source(&self) -> &SourceNamespace {
        &self.source
    }

    pub fn native_session_id(&self) -> &NativeSessionId {
        &self.native_session_id
    }

    pub fn start(&self) -> UtcTimestamp {
        self.start
    }

    pub fn end(&self) -> Option<UtcTimestamp> {
        self.end
    }

    pub fn project_key(&self) -> &ProjectKey {
        &self.project_key
    }

    pub fn repository_key(&self) -> &RepositoryKey {
        &self.repository_key
    }

    pub fn run_id(&self) -> Option<&NativeRunId> {
        self.run_id.as_ref()
    }
}

/// The insert payload for one session row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSession {
    pub source: SourceNamespace,
    pub native_session_id: NativeSessionId,
    pub start: UtcTimestamp,
    pub end: Option<UtcTimestamp>,
    pub project_key: ProjectKey,
    pub repository_key: RepositoryKey,
    pub run_id: Option<NativeRunId>,
}

/// Reads one typed column, mapping the driver error into the store vocabulary.
fn get<T: rusqlite::types::FromSql>(row: &Row<'_>, index: usize) -> Result<T, Error> {
    row.get::<_, T>(index)
        .map_err(|e| Error::Store(format!("cannot read column {index}: {e}")))
}

fn session_from_row(row: &Row<'_>) -> Result<Session, Error> {
    Ok(Session {
        id: SessionDbId::new(get(row, 0)?),
        source: SourceNamespace::new(get::<String>(row, 1)?),
        native_session_id: NativeSessionId::new(get::<String>(row, 2)?),
        start: UtcTimestamp::from_unix_nanos(get(row, 3)?),
        end: get::<Option<i64>>(row, 4)?.map(UtcTimestamp::from_unix_nanos),
        project_key: ProjectKey::new(get::<String>(row, 5)?),
        repository_key: RepositoryKey::new(get::<String>(row, 6)?),
        run_id: get::<Option<String>>(row, 7)?.map(NativeRunId::new),
    })
}

const SESSION_COLUMNS: &str = "id, source, native_session_id, start, end, project_key, \
     repository_key, run_id";

/// Inserts one session row. A duplicate (source, native session id) pair fails at
/// the database rather than being silently merged.
pub fn insert_session(conn: &Connection, session: &NewSession) -> Result<SessionDbId, Error> {
    let id: i64 = conn
        .query_row(
            "INSERT INTO session (
                source, native_session_id, start, end, project_key, repository_key, run_id
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            RETURNING id",
            params![
                session.source.as_str(),
                session.native_session_id.as_str(),
                session.start.unix_nanos(),
                session.end.map(|t| t.unix_nanos()),
                session.project_key.as_str(),
                session.repository_key.as_str(),
                session.run_id.as_ref().map(|id| id.as_str()),
            ],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|e| Error::Store(format!("cannot insert the session row: {e}")))?;
    Ok(SessionDbId::new(id))
}

/// Replaces every session row with the given set, in one transaction: the rebuild
/// path for sessions, which are derived from usage events and therefore rebuildable.
/// Returns the number of rows written.
pub fn replace_all_sessions(
    conn: &mut Connection,
    sessions: &[NewSession],
) -> Result<usize, Error> {
    let tx = conn
        .transaction()
        .map_err(|e| Error::Store(format!("cannot open the session rebuild transaction: {e}")))?;
    tx.execute("DELETE FROM session", [])
        .map_err(|e| Error::Store(format!("cannot clear the session table: {e}")))?;
    let mut written = 0;
    for session in sessions {
        tx.execute(
            "INSERT INTO session (
                source, native_session_id, start, end, project_key, repository_key, run_id
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                session.source.as_str(),
                session.native_session_id.as_str(),
                session.start.unix_nanos(),
                session.end.map(|t| t.unix_nanos()),
                session.project_key.as_str(),
                session.repository_key.as_str(),
                session.run_id.as_ref().map(|id| id.as_str()),
            ],
        )
        .map_err(|e| Error::Store(format!("cannot insert a session row during rebuild: {e}")))?;
        written += 1;
    }
    tx.commit()
        .map_err(|e| Error::Store(format!("cannot commit the session rebuild: {e}")))?;
    Ok(written)
}

/// Maps a store error into the driver error vocabulary at a row boundary, the
/// convention every repository in this module family uses for a domain parse that
/// fails while decoding a stored row.
fn store_error_to_sql(e: Error) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
}

/// Loads every session row, in (source, native session id) order.
pub fn load_all_sessions(conn: &Connection) -> Result<Vec<Session>, Error> {
    let sql = format!("SELECT {SESSION_COLUMNS} FROM session ORDER BY source, native_session_id");
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| Error::Store(format!("cannot prepare the session query: {e}")))?;
    let rows = stmt
        .query_map([], |row| session_from_row(row).map_err(store_error_to_sql))
        .map_err(|e| Error::Store(format!("cannot query session rows: {e}")))?;
    let mut sessions = Vec::new();
    for row in rows {
        let session = row.map_err(|e| Error::Store(format!("cannot read a session row: {e}")))?;
        sessions.push(session);
    }
    Ok(sessions)
}

/// Loads the one session for a namespaced identifier, when it exists.
pub fn load_session(
    conn: &Connection,
    source: &SourceNamespace,
    native_session_id: &NativeSessionId,
) -> Result<Option<Session>, Error> {
    let sql = format!(
        "SELECT {SESSION_COLUMNS} FROM session WHERE source = ?1 AND native_session_id = ?2"
    );
    conn.query_row(
        &sql,
        params![source.as_str(), native_session_id.as_str()],
        |row| session_from_row(row).map_err(store_error_to_sql),
    )
    .optional()
    .map_err(|e| Error::Store(format!("cannot load the session row: {e}")))
}
