//! The `transcript_file` table: the rebuildable transcript index (PLAN.md
//! 12.7, `aub-lqe.2`).
//!
//! One row per (source key, relative path), recording the watermark that
//! decides whether a file is unchanged, was appended to, needs reparsing
//! because the parser changed, or requires a full rebuild. Paths are stored
//! relative to the configured root, which keeps machine-specific absolute
//! paths out of the database.
//!
//! The table is a rebuildable cache: transcripts remain authoritative, so
//! deleting the index only forces a full re-ingest, and this repository
//! exposes exactly one delete, the whole-index rebuild path. It is
//! deliberately not projection-relevant meter state, so its writes do not
//! advance the ledger generation.

use rusqlite::{OptionalExtension, params};

use crate::error::Error;
use crate::transcripts::watermark::Watermark;

/// Creates the table this migration owns.
///
/// Called once, from the migration that introduces this table
/// (`0006_transcript_file.rs`). Never called again: a later migration that
/// touched this table would violate the forward-only rule.
pub(crate) fn create_table(conn: &rusqlite::Connection) -> Result<(), Error> {
    conn.execute_batch(
        "CREATE TABLE transcript_file (
            source_key TEXT NOT NULL,
            relative_path TEXT NOT NULL,
            size INTEGER NOT NULL CHECK (size >= 0),
            mtime_nanos INTEGER NOT NULL,
            identity TEXT NOT NULL,
            parser_version TEXT NOT NULL,
            consumed_offset INTEGER NOT NULL CHECK (consumed_offset >= 0),
            PRIMARY KEY (source_key, relative_path)
        ) STRICT",
    )
    .map_err(|e| Error::Store(format!("cannot create the transcript_file table: {e}")))
}

fn row_to_watermark(row: &rusqlite::Row<'_>) -> rusqlite::Result<Watermark> {
    Ok(Watermark {
        source_key: row.get(0)?,
        relative_path: row.get(1)?,
        // Read as u64 directly: rusqlite refuses a negative INTEGER for u64,
        // so a corrupted row fails loudly instead of clamping to zero.
        size: row.get(2)?,
        mtime_nanos: row.get(3)?,
        identity: row.get(4)?,
        parser_version: row.get(5)?,
        consumed_offset: row.get(6)?,
    })
}

/// Records or replaces the watermark for one file.
///
/// The row is replaced rather than accumulated: the index holds the latest
/// state of each file, and nothing about an older watermark is worth
/// reconstructing. The path must be relative to the configured root: an
/// absolute path would put a machine-specific identity into the database,
/// which is exactly what the no-compiled-identity rule forbids at the data
/// layer as well as the code layer.
pub fn upsert(conn: &rusqlite::Connection, watermark: &Watermark) -> Result<(), Error> {
    if std::path::Path::new(&watermark.relative_path).is_absolute() {
        return Err(Error::Store(format!(
            "transcript file path must be relative to the configured root, got {}",
            watermark.relative_path
        )));
    }
    let size = i64::try_from(watermark.size).map_err(|_| {
        Error::Store(format!(
            "transcript file size {} is outside the representable range",
            watermark.size
        ))
    })?;
    let consumed_offset = i64::try_from(watermark.consumed_offset).map_err(|_| {
        Error::Store(format!(
            "transcript file consumed offset {} is outside the representable range",
            watermark.consumed_offset
        ))
    })?;
    conn.execute(
        "INSERT INTO transcript_file
            (source_key, relative_path, size, mtime_nanos, identity, parser_version, consumed_offset)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT (source_key, relative_path) DO UPDATE SET
            size = excluded.size,
            mtime_nanos = excluded.mtime_nanos,
            identity = excluded.identity,
            parser_version = excluded.parser_version,
            consumed_offset = excluded.consumed_offset",
        params![
            watermark.source_key,
            watermark.relative_path,
            size,
            watermark.mtime_nanos,
            watermark.identity,
            watermark.parser_version,
            consumed_offset,
        ],
    )
    .map_err(|e| Error::Store(format!("cannot record the transcript file watermark: {e}")))?;
    Ok(())
}

/// Reads the stored watermark for one file, or `None` when the file is not
/// indexed.
pub fn watermark_for(
    conn: &rusqlite::Connection,
    source_key: &str,
    relative_path: &str,
) -> Result<Option<Watermark>, Error> {
    conn.query_row(
        "SELECT source_key, relative_path, size, mtime_nanos, identity, parser_version, consumed_offset
         FROM transcript_file WHERE source_key = ?1 AND relative_path = ?2",
        params![source_key, relative_path],
        row_to_watermark,
    )
    .optional()
    .map_err(|e| Error::Store(format!("cannot read the transcript file watermark: {e}")))
}

/// Reads every stored watermark, in source and path order.
pub fn all_watermarks(conn: &rusqlite::Connection) -> Result<Vec<Watermark>, Error> {
    let mut stmt = conn
        .prepare(
            "SELECT source_key, relative_path, size, mtime_nanos, identity, parser_version, consumed_offset
             FROM transcript_file ORDER BY source_key, relative_path",
        )
        .map_err(|e| Error::Store(format!("cannot prepare the watermark read: {e}")))?;
    let rows = stmt
        .query_map([], row_to_watermark)
        .map_err(|e| Error::Store(format!("cannot read the transcript file watermarks: {e}")))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| Error::Store(format!("cannot read the transcript file watermarks: {e}")))
}

/// Deletes the whole index: the rebuild path. The table is a rebuildable
/// cache, so this is the one delete this repository exposes; deleting it
/// makes every discovered file classify as new.
pub fn delete_all(conn: &rusqlite::Connection) -> Result<(), Error> {
    conn.execute("DELETE FROM transcript_file", [])
        .map_err(|e| Error::Store(format!("cannot delete the transcript file index: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::connection::{AccessMode, PragmaPolicy, open};
    use crate::transcripts::watermark::{
        ChangeClass, FileState, classify, last_complete_line_offset,
    };
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new() -> Self {
            let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "aub-store-transcript-file-test-{}-{suffix}",
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

    /// A database with the transcript_file table in place, opened for write.
    ///
    /// The table is created through the same `create_table` the migration
    /// calls, so the test exercises the table definition production gets
    /// rather than a hand-written copy of it.
    fn fixture_conn() -> (ScratchDir, rusqlite::Connection) {
        let scratch = ScratchDir::new();
        let db_path = scratch.path().join("meter.db");
        let policy = PragmaPolicy {
            busy_timeout: crate::domain::time::MonotonicDuration::from_millis(1000),
        };
        let mut conn = open(&db_path, AccessMode::ReadWrite, &policy).unwrap();
        create_table(&conn).unwrap();
        (scratch, conn)
    }

    fn sample(source_key: &str, relative_path: &str) -> Watermark {
        Watermark {
            source_key: source_key.to_string(),
            relative_path: relative_path.to_string(),
            size: 100,
            mtime_nanos: 1_000,
            identity: "dev:7".to_string(),
            parser_version: "parser-1".to_string(),
            consumed_offset: 100,
        }
    }

    /// A recorded watermark reads back exactly as written.
    #[test]
    fn a_recorded_watermark_reads_back_exactly() {
        let (_scratch, conn) = fixture_conn();
        let watermark = sample("claude-code", "session.jsonl");
        upsert(&conn, &watermark).unwrap();

        let read = watermark_for(&conn, "claude-code", "session.jsonl")
            .unwrap()
            .unwrap();
        assert_eq!(read, watermark);
    }

    /// The planted negative for the primary key: the same (source, path) pair
    /// is one row, replaced rather than accumulated.
    #[test]
    fn the_same_source_and_path_is_one_row_replaced_not_accumulated() {
        let (_scratch, conn) = fixture_conn();
        upsert(&conn, &sample("claude-code", "session.jsonl")).unwrap();
        let mut updated = sample("claude-code", "session.jsonl");
        updated.size = 200;
        updated.consumed_offset = 200;
        upsert(&conn, &updated).unwrap();

        let all = all_watermarks(&conn).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].size, 200);
        assert_eq!(all[0].consumed_offset, 200);
    }

    /// Identical relative paths under different sources are distinct rows: the
    /// primary key is the pair, not the path alone.
    #[test]
    fn identical_paths_under_different_sources_are_distinct_rows() {
        let (_scratch, conn) = fixture_conn();
        upsert(&conn, &sample("claude-code", "session.jsonl")).unwrap();
        upsert(&conn, &sample("codex", "session.jsonl")).unwrap();

        let all = all_watermarks(&conn).unwrap();
        assert_eq!(all.len(), 2);
    }

    /// A file with no stored watermark reads as `None`, which the classifier
    /// turns into `New`.
    #[test]
    fn an_unindexed_file_reads_as_none() {
        let (_scratch, conn) = fixture_conn();
        assert!(
            watermark_for(&conn, "claude-code", "missing.jsonl")
                .unwrap()
                .is_none()
        );
    }

    /// Deleting the index makes every file classify as new again, which is
    /// the rebuild contract.
    #[test]
    fn deleting_the_index_makes_every_file_new_again() {
        let (_scratch, conn) = fixture_conn();
        upsert(&conn, &sample("claude-code", "session.jsonl")).unwrap();
        delete_all(&conn).unwrap();

        let stored = watermark_for(&conn, "claude-code", "session.jsonl").unwrap();
        let current = crate::transcripts::watermark::FileState {
            size: 100,
            mtime_nanos: 1_000,
            identity: "dev:7".to_string(),
        };
        assert_eq!(
            crate::transcripts::watermark::classify(stored.as_ref(), &current, "parser-1"),
            ChangeClass::New
        );
    }

    /// A negative consumed offset is unrepresentable in the table: the CHECK
    /// refuses it at the database rather than at the API.
    #[test]
    fn a_negative_consumed_offset_is_rejected() {
        let (_scratch, conn) = fixture_conn();
        let err = conn
            .execute(
                "INSERT INTO transcript_file
                    (source_key, relative_path, size, mtime_nanos, identity, parser_version, consumed_offset)
                 VALUES ('claude-code', 'session.jsonl', 100, 1000, 'dev:7', 'parser-1', -1)",
                [],
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("consumed_offset"),
            "expected the failure to name the rejected column: {err}"
        );
    }

    /// An absolute path is refused at the API: a machine-specific path must
    /// never enter the table, which is the data-layer half of the
    /// no-compiled-identity rule.
    #[test]
    fn an_absolute_path_is_refused() {
        let (_scratch, conn) = fixture_conn();
        let mut watermark = sample("claude-code", "session.jsonl");
        watermark.relative_path = "/home/gabriel/.claude/projects/session.jsonl".to_string();
        let err = upsert(&conn, &watermark).unwrap_err();
        assert!(
            err.to_string().contains("relative to the configured root"),
            "expected the refusal to name the relative-path rule: {err}"
        );
        assert!(
            all_watermarks(&conn).unwrap().is_empty(),
            "the refused watermark must not land in the table"
        );
    }

    // --- the ingest flow, with a stand-in parse ---------------------------------
    //
    // The real parse and the ingest orchestration belong to aub-lqe.11; these
    // tests compose the pieces this bead delivers (discovery state, classify,
    // watermark persistence) with a stand-in parse that counts calls, which is
    // what makes "no parsing on the second run" measurable here.

    /// One ingest pass over a corpus directory: classify every file, "parse"
    /// the ones that are not unchanged, and record their watermarks. Returns
    /// the change classes in deterministic order and the number of parses.
    fn ingest_pass(
        conn: &rusqlite::Connection,
        dir: &Path,
        source_key: &str,
        parser_version: &str,
        parse_count: &mut u64,
    ) -> Vec<ChangeClass> {
        let mut classes = Vec::new();
        let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        entries.sort();
        for path in entries {
            let relative = path
                .strip_prefix(dir)
                .unwrap()
                .to_string_lossy()
                .to_string();
            let stored = watermark_for(conn, source_key, &relative).unwrap();
            let current = FileState::read(&path).unwrap();
            let class = classify(stored.as_ref(), &current, parser_version);
            classes.push(class);
            if class == ChangeClass::Unchanged {
                continue;
            }
            *parse_count += 1;
            let content = std::fs::read_to_string(&path).unwrap();
            let offset = last_complete_line_offset(&content, content.len() as u64);
            upsert(
                conn,
                &Watermark {
                    source_key: source_key.to_string(),
                    relative_path: relative,
                    size: current.size,
                    mtime_nanos: current.mtime_nanos,
                    identity: current.identity,
                    parser_version: parser_version.to_string(),
                    consumed_offset: offset,
                },
            )
            .unwrap();
        }
        classes
    }

    /// A scratch corpus directory holding two complete JSONL files.
    fn corpus_dir(scratch: &ScratchDir) -> PathBuf {
        let dir = scratch.path().join("corpus");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("a.jsonl"), "{\"n\":1}\n{\"n\":2}\n").unwrap();
        std::fs::write(dir.join("b.jsonl"), "{\"n\":3}\n").unwrap();
        dir
    }

    /// Ingesting an unchanged corpus twice performs no parsing on the second
    /// run: every file classifies as unchanged and the parse counter stays put.
    #[test]
    fn an_unchanged_corpus_performs_no_parsing_on_the_second_run() {
        let (scratch, conn) = fixture_conn();
        let dir = corpus_dir(&scratch);
        let mut parses = 0;

        let first = ingest_pass(&conn, &dir, "claude-code", "parser-1", &mut parses);
        assert_eq!(first, vec![ChangeClass::New, ChangeClass::New]);
        assert_eq!(parses, 2, "the first run parses every file");

        let second = ingest_pass(&conn, &dir, "claude-code", "parser-1", &mut parses);
        assert_eq!(second, vec![ChangeClass::Unchanged, ChangeClass::Unchanged]);
        assert_eq!(parses, 2, "the second run must perform no parsing");
    }

    /// A parser version change between runs forces a reparse of every file,
    /// even though none of them changed on disk.
    #[test]
    fn a_parser_version_change_reparses_the_whole_corpus() {
        let (scratch, conn) = fixture_conn();
        let dir = corpus_dir(&scratch);
        let mut parses = 0;

        ingest_pass(&conn, &dir, "claude-code", "parser-1", &mut parses);
        let second = ingest_pass(&conn, &dir, "claude-code", "parser-2", &mut parses);
        assert_eq!(
            second,
            vec![
                ChangeClass::ParserVersionChanged,
                ChangeClass::ParserVersionChanged
            ]
        );
        assert_eq!(parses, 4, "the parser change must reparse every file");
    }

    /// Deleting the index and re-ingesting the same corpus recreates the same
    /// watermarks, and every file classifies as new again: the rebuild
    /// contract, over generated corpora.
    #[test]
    fn deleting_the_index_and_reingesting_recreates_the_same_watermarks() {
        use test_support::{Rng, Seed, check_property};

        check_property("rebuild determinism", 0..32, |seed| {
            let scratch = ScratchDir::new();
            let dir = scratch.path().join("corpus");
            std::fs::create_dir(&dir).unwrap();
            let mut rng = Rng::new(Seed(seed));
            let file_count = 1 + rng.next_below(4) as usize;
            for index in 0..file_count {
                let mut content = String::new();
                let lines = 1 + rng.next_below(5);
                for _ in 0..lines {
                    content.push_str(&format!("{{\"n\":{}}}\n", rng.next_below(1000)));
                }
                std::fs::write(dir.join(format!("f{index}.jsonl")), content).unwrap();
            }

            let db_path = scratch.path().join("meter.db");
            let policy = PragmaPolicy {
                busy_timeout: crate::domain::time::MonotonicDuration::from_millis(1000),
            };
            let conn = open(&db_path, AccessMode::ReadWrite, &policy).unwrap();
            create_table(&conn).unwrap();

            let mut parses = 0;
            let first = ingest_pass(&conn, &dir, "claude-code", "parser-1", &mut parses);
            let watermarks_before = all_watermarks(&conn).unwrap();

            delete_all(&conn).unwrap();
            let mut parses_after = 0;
            let second = ingest_pass(&conn, &dir, "claude-code", "parser-1", &mut parses_after);
            let watermarks_after = all_watermarks(&conn).unwrap();

            let all_new = vec![ChangeClass::New; file_count];
            if first != all_new || second != all_new {
                return false;
            }
            if watermarks_before != watermarks_after {
                return false;
            }
            if parses != parses_after {
                return false;
            }
            true
        });
    }
}
