//! opencode transcript rendering behind `aub export transcript` (aub-m76e).
//!
//! These tests prove the database-to-markdown path the unit tests cannot: a
//! scratch `opencode.db` is built from the committed seed
//! (`tests/fixtures/transcripts/opencode/transcript_seed.json`), read back
//! through the store's one opencode connection function and transcript query,
//! formatted into the renderer's interchange lines, and compared against the
//! three committed goldens. The live database is never read here.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use agent_usage_book::presentation::transcript::{
    OpencodeTranscriptRenderer, TranscriptRenderOptions, TranscriptRenderer, opencode_line,
    render_transcript_markdown,
};
use agent_usage_book::store::opencode::{
    open_opencode_database, opencode_session_exists, read_session_transcript_rows,
};

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new(tag: &str) -> Self {
        let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "aub-opencode-render-{tag}-{}-{suffix}",
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

fn seed_value() -> serde_json::Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/transcripts/opencode/transcript_seed.json");
    let text = std::fs::read_to_string(&path).expect("transcript seed must be readable");
    serde_json::from_str(&text).expect("transcript seed must parse as JSON")
}

fn golden(name: &str) -> String {
    std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/transcripts/opencode")
            .join(name),
    )
    .unwrap_or_else(|_| panic!("the opencode golden {name} must exist"))
}

/// Builds a scratch opencode database from the committed seed, with the real
/// table shapes beside the seeded rows, in WAL mode so the concurrent-writer
/// test reads a database shaped like the live one. Returns the database path.
fn build_fixture_db(dir: &Path, name: &str) -> PathBuf {
    let db_path = dir.join(name);
    let conn = rusqlite::Connection::open(&db_path).expect("fixture db must open");
    conn.execute_batch(
        "CREATE TABLE session (id TEXT PRIMARY KEY, project_id TEXT NOT NULL, \
         directory TEXT NOT NULL, title TEXT NOT NULL, \
         time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL); \
         CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, \
         time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL); \
         CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT NOT NULL, \
         session_id TEXT NOT NULL, time_created INTEGER NOT NULL, \
         time_updated INTEGER NOT NULL, data TEXT NOT NULL);",
    )
    .expect("fixture tables must create");
    let seed = seed_value();
    for session in seed["sessions"]
        .as_array()
        .expect("seed must hold sessions")
    {
        conn.execute(
            "INSERT INTO session (id, project_id, directory, title, time_created, time_updated) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                session["id"].as_str().expect("session id"),
                session["project_id"].as_str().expect("project id"),
                session["directory"].as_str().expect("directory"),
                session["title"].as_str().expect("title"),
                session["time_created_ms"].as_i64().expect("created"),
                session["time_updated_ms"].as_i64().expect("updated"),
            ],
        )
        .expect("fixture session must insert");
    }
    for message in seed["messages"]
        .as_array()
        .expect("seed must hold messages")
    {
        let data = serde_json::to_string(&message["data"]).expect("data must serialize");
        conn.execute(
            "INSERT INTO message (id, session_id, time_created, time_updated, data) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                message["id"].as_str().expect("message id"),
                message["session_id"].as_str().expect("session id"),
                message["time_created_ms"].as_i64().expect("created"),
                message["time_updated_ms"].as_i64().expect("updated"),
                data,
            ],
        )
        .expect("fixture message must insert");
    }
    for part in seed["parts"].as_array().expect("seed must hold parts") {
        let data = serde_json::to_string(&part["data"]).expect("data must serialize");
        conn.execute(
            "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                part["id"].as_str().expect("part id"),
                part["message_id"].as_str().expect("message id"),
                part["session_id"].as_str().expect("session id"),
                part["time_created_ms"].as_i64().expect("created"),
                part["time_updated_ms"].as_i64().expect("updated"),
                data,
            ],
        )
        .expect("fixture part must insert");
    }
    conn.pragma_update(None, "journal_mode", "WAL")
        .expect("fixture db must enter WAL mode");
    drop(conn);
    db_path
}

/// Reads one session from the database at `db_path` through the store's
/// read-only connection function and transcript query, and renders the rows
/// through the registered opencode renderer, the way the export command does.
fn render_session(
    db_path: &Path,
    session_id: &str,
    include_tools: bool,
    include_thinking: bool,
) -> String {
    let connection = open_opencode_database(db_path).expect("fixture db must open read-only");
    let rows = read_session_transcript_rows(&connection, session_id).expect("transcript must read");
    let mut body = String::new();
    for row in &rows {
        body.push_str(&opencode_line(&row.message_id, &row.role, &row.part_data));
        body.push('\n');
    }
    let (messages, _) = OpencodeTranscriptRenderer.render_file_with_skipped(&body);
    let document = agent_usage_book::presentation::transcript::TranscriptDocument {
        harness: "opencode".to_string(),
        project: "fixture-project".to_string(),
        session_id: session_id.to_string(),
        started: agent_usage_book::domain::time::UtcTimestamp::from_unix_nanos(
            1_788_220_900_000_000_000,
        ),
        files: vec![agent_usage_book::presentation::transcript::TranscriptFile {
            file_name: String::new(),
            is_subagent: false,
            messages,
        }],
    };
    render_transcript_markdown(
        &document,
        &TranscriptRenderOptions {
            include_tools,
            include_thinking,
        },
    )
}

/// The three goldens pin the full database-to-markdown path: seed file to
/// scratch database to store query to renderer.
#[test]
fn golden_plain_renders_the_fixture_database() {
    let scratch = ScratchDir::new("golden");
    let db_path = build_fixture_db(scratch.path(), "opencode.db");
    assert_eq!(
        render_session(&db_path, "ses_m76e_fixture", false, false),
        golden("plain.md")
    );
}

#[test]
fn golden_tools_renders_the_fixture_database() {
    let scratch = ScratchDir::new("golden");
    let db_path = build_fixture_db(scratch.path(), "opencode.db");
    assert_eq!(
        render_session(&db_path, "ses_m76e_fixture", true, false),
        golden("tools.md")
    );
}

#[test]
fn golden_thinking_renders_the_fixture_database() {
    let scratch = ScratchDir::new("golden");
    let db_path = build_fixture_db(scratch.path(), "opencode.db");
    assert_eq!(
        render_session(&db_path, "ses_m76e_fixture", false, true),
        golden("thinking.md")
    );
}

/// The transcript query returns parts in conversation order: messages by row
/// time, parts within a message by theirs, carrying the message role beside
/// each part row.
#[test]
fn transcript_rows_come_back_in_conversation_order_with_roles() {
    let scratch = ScratchDir::new("order");
    let db_path = build_fixture_db(scratch.path(), "opencode.db");
    let connection = open_opencode_database(&db_path).expect("fixture db must open read-only");
    let rows = read_session_transcript_rows(&connection, "ses_m76e_fixture").expect("must read");
    let order: Vec<(&str, &str)> = rows
        .iter()
        .map(|row| (row.message_id.as_str(), row.role.as_str()))
        .collect();
    assert_eq!(
        order,
        vec![
            ("msg_m76e_user", "user"),
            ("msg_m76e_tool", "assistant"),
            ("msg_m76e_tool", "assistant"),
            ("msg_m76e_tool", "assistant"),
            ("msg_m76e_tool", "assistant"),
            ("msg_m76e_think", "assistant"),
            ("msg_m76e_think", "assistant"),
            ("msg_m76e_think", "assistant"),
            ("msg_m76e_think", "assistant"),
        ]
    );
}

/// A session id the database no longer holds reads as no rows with no session
/// row: the export tells that apart from an empty session and reports the id
/// rather than rendering an empty document.
#[test]
fn a_pruned_session_reads_as_no_rows_and_no_session_row() {
    let scratch = ScratchDir::new("pruned");
    let db_path = build_fixture_db(scratch.path(), "opencode.db");
    let connection = open_opencode_database(&db_path).expect("fixture db must open read-only");
    let rows = read_session_transcript_rows(&connection, "ses_pruned_away").expect("must read");
    assert!(rows.is_empty());
    assert!(
        !opencode_session_exists(&connection, "ses_pruned_away").expect("must read"),
        "a pruned id has no session row"
    );
    // The planted negative: the fixture session itself must read the other
    // way, or this test would pass on a query that returns nothing at all.
    assert!(
        opencode_session_exists(&connection, "ses_m76e_fixture").expect("must read"),
        "the fixture session has its session row"
    );
    assert!(
        !read_session_transcript_rows(&connection, "ses_m76e_fixture")
            .expect("must read")
            .is_empty()
    );
}

/// The transcript connection is read-only: a write through it is refused by
/// SQLite, so the export can never mark the operator's live database.
#[test]
fn the_transcript_connection_refuses_writes() {
    let scratch = ScratchDir::new("readonly");
    let db_path = build_fixture_db(scratch.path(), "opencode.db");
    let connection = open_opencode_database(&db_path).expect("fixture db must open read-only");
    let refused = connection.execute("CREATE TABLE probe (id INTEGER)", []);
    match refused {
        Err(error) => assert!(
            error.to_string().contains("readonly"),
            "a write through the transcript connection must be refused as readonly, got {error}"
        ),
        Ok(_) => panic!("a write through the transcript connection must be refused"),
    }
}

/// Reading a session while a writer holds the write slot still renders: the
/// read-only connection reads the last committed snapshot without waiting for
/// the writer, the same guarantee the usage reader leans on.
#[test]
fn rendering_while_a_writer_holds_the_write_slot_still_renders() {
    let scratch = ScratchDir::new("concurrent");
    let db_path = build_fixture_db(scratch.path(), "opencode.db");
    let mut writer = rusqlite::Connection::open(&db_path).expect("writer must open");
    let tx = writer
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .expect("writer must take the write slot");
    tx.execute(
        "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) \
         VALUES ('prt_m76e_held', 'msg_m76e_think', 'ses_m76e_fixture', 1788220995000, 1788220995000, '{}')",
        [],
    )
    .expect("held insert must run");
    // The insert is uncommitted while the reader reads, so the snapshot shows
    // the pre-insert state; what matters is that the read completes instead
    // of blocking behind the held slot.
    let rendered = render_session(&db_path, "ses_m76e_fixture", false, false);
    assert_eq!(rendered, golden("plain.md"));
    tx.rollback().expect("held transaction must roll back");
}
