//! Integration tests for the external-friction-ledger export join (`aub-xus.7`).
//!
//! These drive the whole path a real export drives: the store assembly, the
//! report model, and the JSONL renderer, against a real migrated database. The
//! unit tests of each layer pin their own piece; this file pins the join
//! contract that crosses all three: the run identifier comes out of an export
//! exactly as it went in, so an external ledger joins on it with no
//! transformation, and nothing but the chosen identifiers and the usage leaves
//! the database.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use agent_usage_book::domain::ids::{NativeRunId, NativeSessionId, SourceNamespace};
use agent_usage_book::domain::time::{FakeClock, MonotonicDuration, UtcTimestamp};
use agent_usage_book::presentation::export_jsonl::export_jsonl;
use agent_usage_book::report::export::assemble;
use agent_usage_book::sessions::resolver::{ProjectKey, RepositoryKey};
use agent_usage_book::store::connection::{AccessMode, PragmaPolicy, open};
use agent_usage_book::store::export::ExportKey;
use agent_usage_book::store::ingest_quarantine::{NewQuarantineItem, record_quarantine};
use agent_usage_book::store::migrate::run_migrations;
use agent_usage_book::store::migrations::registry;
use agent_usage_book::store::session::{NewSession, insert_session};
use agent_usage_book::store::usage_component::{NewUsageComponent, insert_component};
use agent_usage_book::store::usage_event::{EventId, NewUsageEvent, insert_event};

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TestDb {
    path: PathBuf,
}

impl TestDb {
    fn new() -> Self {
        let count = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "aub-test-export-{}-{count}.sqlite3",
            std::process::id()
        ));
        Self { path }
    }

    fn open(&self) -> rusqlite::Connection {
        let policy = PragmaPolicy {
            busy_timeout: MonotonicDuration::from_millis(5000),
        };
        let mut conn = open(&self.path, AccessMode::ReadWrite, &policy).unwrap();
        run_migrations(
            &mut conn,
            &registry(),
            None,
            &FakeClock::new(UtcTimestamp::from_unix_nanos(0)),
        )
        .unwrap();
        conn
    }
}

impl Drop for TestDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Seeds one namespaced session and one usage event with the given token
/// counts, so a test can build a small ledger in a few lines.
fn seed_session_with_usage(
    conn: &rusqlite::Connection,
    index: usize,
    source: &str,
    native: &str,
    run: Option<&str>,
    provenance: &str,
    counts: &[(&str, u64)],
) {
    insert_session(
        conn,
        &NewSession {
            source: SourceNamespace::new(source),
            native_session_id: NativeSessionId::new(native),
            start: UtcTimestamp::from_unix_nanos(100 * index as i64),
            end: Some(UtcTimestamp::from_unix_nanos(100 * index as i64 + 50)),
            project_key: ProjectKey::new(format!("proj-{index}")),
            repository_key: RepositoryKey::new(format!("repo-{index}")),
            run_id: run.map(NativeRunId::new),
        },
    )
    .unwrap();
    let event_id = insert_event(
        conn,
        &NewUsageEvent {
            canonical_event_id: Box::leak(format!("ce-{index}").into_boxed_str()),
            session_id: Some(Box::leak(native.to_string().into_boxed_str())),
            event_timestamp: Some(UtcTimestamp::from_unix_nanos(100 * index as i64)),
            model_id: None,
            evidence_kind: "transcript",
            source_provenance: Box::leak(provenance.to_string().into_boxed_str()),
            parser_version: "v1",
            created_at: UtcTimestamp::from_unix_nanos(100 * index as i64),
        },
    )
    .unwrap();
    for (token_class, count) in counts {
        insert_component(
            conn,
            &NewUsageComponent {
                event_id,
                token_class: Box::leak((*token_class).to_string().into_boxed_str()),
                count: *count,
            },
        )
        .unwrap();
    }
}

/// Two sessions sharing one run id across two sources, plus a run-less
/// session: the smallest fixture that exercises both key modes at once.
fn seed_two_sources_one_run(conn: &rusqlite::Connection) {
    seed_session_with_usage(
        conn,
        0,
        "claude-code",
        "sess-a",
        Some("run-friction-1"),
        "transcripts/a.jsonl",
        &[("input", 100), ("output", 40)],
    );
    seed_session_with_usage(
        conn,
        1,
        "codex",
        "sess-b",
        Some("run-friction-1"),
        "transcripts/b.jsonl",
        &[("input", 200), ("output", 60)],
    );
    seed_session_with_usage(
        conn,
        2,
        "claude-code",
        "sess-c",
        None,
        "transcripts/c.jsonl",
        &[("input", 7)],
    );
}

/// Both key modes produce one JSON object per line, the header carries the
/// export format version and both generations, and the run-keyed rows carry
/// the stored run identifier verbatim: the join key a friction ledger reads
/// with no transformation.
#[test]
fn both_key_modes_produce_one_object_per_line_with_version_and_generations() {
    let db = TestDb::new();
    let conn = db.open();
    seed_two_sources_one_run(&conn);

    for (key, key_name) in [
        (ExportKey::Session, "session-id"),
        (ExportKey::Run, "run-id"),
    ] {
        let report = assemble(&conn, key, true, UtcTimestamp::from_unix_nanos(1_000)).unwrap();
        let rendered = export_jsonl(&report);
        let lines: Vec<&str> = rendered.lines().collect();
        assert!(lines.len() >= 2, "{key_name}: header plus at least one row");

        let header: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(header["schema"], 1, "{key_name}: versioned header");
        assert_eq!(header["key"], key_name);
        assert_eq!(header["ledger_generation"], 0);
        assert_eq!(header["ingestion_generation"], 0);
        assert_eq!(
            header["included_identifiers"],
            serde_json::json!(["project", "repository"])
        );
        for line in &lines {
            let parsed: serde_json::Value = serde_json::from_str(line)
                .unwrap_or_else(|error| panic!("{key_name}: line is one JSON object: {error}"));
            assert!(parsed.is_object());
        }

        if key == ExportKey::Run {
            // The join key arrives exactly as it was stored: a friction ledger
            // reads `key` and finds the same bytes its own run column holds.
            assert!(
                lines[1..]
                    .iter()
                    .all(|line| line.contains("run-friction-1")),
                "every run-keyed row carries the stored run id verbatim"
            );
            let total_input: u64 = lines[1..]
                .iter()
                .map(|line| {
                    let row: serde_json::Value = serde_json::from_str(line).unwrap();
                    row["usage"]["input"]["value"]
                        .as_str()
                        .unwrap()
                        .parse::<u64>()
                        .unwrap()
                })
                .sum();
            assert_eq!(total_input, 300, "usage sums across the two sessions");
            let runless = lines[1..].iter().any(|line| line.contains("sess-c"));
            assert!(
                !runless,
                "a session without a run contributes no run-keyed row"
            );
        }
    }
}

/// Re-rendering an export over unchanged data changes exactly one thing: the
/// volatile `generated_at`. Every other byte is identical. The test also
/// proves the volatile field actually moved, so the normalization it applies
/// is load-bearing rather than papering over two identical strings.
#[test]
fn re_rendering_unchanged_data_differs_only_in_generated_at() {
    let db = TestDb::new();
    let conn = db.open();
    seed_two_sources_one_run(&conn);

    let first = assemble(
        &conn,
        ExportKey::Run,
        true,
        UtcTimestamp::from_unix_nanos(1_000),
    )
    .unwrap();
    let second = assemble(
        &conn,
        ExportKey::Run,
        true,
        UtcTimestamp::from_unix_nanos(9_000),
    )
    .unwrap();
    let first_rendered = export_jsonl(&first);
    let second_rendered = export_jsonl(&second);

    let first_header: serde_json::Value =
        serde_json::from_str(first_rendered.lines().next().unwrap()).unwrap();
    let second_header: serde_json::Value =
        serde_json::from_str(second_rendered.lines().next().unwrap()).unwrap();
    assert_ne!(
        first_header["generated_at"], second_header["generated_at"],
        "the clock moved between the two renders; a fixed reading here would \
         make the normalization below vacuous"
    );

    // Every header field other than generated_at is identical.
    for field in [
        "schema",
        "key",
        "ledger_generation",
        "ingestion_generation",
        "included_identifiers",
        "unresolved_events",
    ] {
        assert_eq!(
            first_header[field], second_header[field],
            "{field} must be a function of the ledger state, not of the clock"
        );
    }

    // Every record line is byte-identical.
    assert_eq!(
        first_rendered.lines().skip(1).collect::<Vec<_>>(),
        second_rendered.lines().skip(1).collect::<Vec<_>>(),
        "records are identical; only the header's generated_at may differ"
    );
    assert_eq!(first_header["generated_at"], 1_000);
    assert_eq!(second_header["generated_at"], 9_000);
}

// The privacy scan (`aub-xus.7`): over generated exports, no credential
// material, no absolute machine path and no transcript content appears. The
// adversarial material is planted in the places it genuinely lives: transcript
// file paths and credential-shaped strings in `usage_event.source_provenance`,
// transcript prose in the quarantine excerpts. A naive implementation that
// renders provenance alongside its rows, or walks the quarantine table, fails
// this scan; the near-identical positive control asserts the legitimate
// identifiers the same export was asked to include.
// The doc comment above is prose for the reader, not rustdoc: the attribute
// below is consumed by the proptest macro, which cannot carry documentation.
proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig::with_cases(16))]

    #[test]
    fn no_forbidden_material_in_any_rendered_export(
        session_count in 1usize..4,
        path_seed in proptest::prelude::any::<u64>(),
        prose_seed in proptest::prelude::any::<u64>(),
        credential_seed in proptest::prelude::any::<u64>(),
    ) {
        let forbidden: Vec<String> = (0..session_count)
            .flat_map(|i| {
                [
                    format!(
                        "/home/user{}/.claude/projects/p{i}/session-{i}.jsonl",
                        path_seed % 97
                    ),
                    format!(
                        "The agent reasoned about topic {prose_seed}{i} and wrote {prose_seed}{i} characters of prose"
                    ),
                    format!("sk-ant-api03-credential-{}{i}{i}", credential_seed % 97),
                ]
            })
            .collect();

            let db = TestDb::new();
            let conn = db.open();
            for i in 0..session_count {
                seed_session_with_usage(
                    &conn,
                    i,
                    "claude-code",
                    &format!("sess-{i}"),
                    Some("run-privacy"),
                    // The provenance the real ingest writes is the transcript
                    // file path; the credential string rides in a second row
                    // so both forbidden classes sit where a naive row renderer
                    // would pick them up.
                    &format!(
                        "/home/user{}/.claude/projects/p{i}/session-{i}.jsonl",
                        path_seed % 97
                    ),
                    &[("input", 10 + i as u64)],
                );
                let event_id = EventId::new((i + 1) as i64);
                insert_component(
                    &conn,
                    &NewUsageComponent { event_id, token_class: "output", count: 5 },
                )
                .unwrap();
                record_quarantine(
                    &conn,
                    &NewQuarantineItem {
                        source_file: format!(
                            "/home/user{}/.claude/projects/p{i}/session-{i}.jsonl",
                            path_seed % 97
                        ),
                        byte_offset: Some(0),
                        line_number: Some(i as u64),
                        parser: "v1".to_string(),
                        failure_class: "malformed-record".to_string(),
                        excerpt_hash: format!("hash-{i}"),
                        excerpt: Some(format!(
                            "The agent reasoned about topic {prose_seed}{i} and wrote {prose_seed}{i} characters of prose"
                        )),
                        observed_at: UtcTimestamp::from_unix_nanos(1),
                    },
                )
                .unwrap();
            }
            // A credential-shaped string planted in provenance, alongside the
            // paths: a field a naive export might render as row context.
            let leak = insert_event(
                &conn,
                &NewUsageEvent {
                    canonical_event_id: "ce-credential",
                    session_id: Some("sess-0"),
                    event_timestamp: Some(UtcTimestamp::from_unix_nanos(10)),
                    model_id: None,
                    evidence_kind: "transcript",
                    source_provenance: &format!("sk-ant-api03-credential-{}", credential_seed % 97),
                    parser_version: "v1",
                    created_at: UtcTimestamp::from_unix_nanos(10),
                },
            )
            .unwrap();
            insert_component(
                &conn,
                &NewUsageComponent { event_id: leak, token_class: "input", count: 1 },
            )
            .unwrap();

            for key in [ExportKey::Session, ExportKey::Run] {
                let report = assemble(&conn, key, true, UtcTimestamp::from_unix_nanos(2_000)).unwrap();
                let rendered = export_jsonl(&report);
                for forbidden in &forbidden {
                    assert!(
                        !rendered.contains(forbidden.as_str()),
                        "{key:?}: forbidden material reached the export: {forbidden:?}"
                    );
                }
            }

            // The positive control: the identifiers the export was asked to
            // include are genuinely present, so the scan above constrains
            // exactly the forbidden material and nothing else.
            let report = assemble(&conn, ExportKey::Session, true, UtcTimestamp::from_unix_nanos(2_000)).unwrap();
            let rendered = export_jsonl(&report);
            assert!(rendered.contains("proj-0"), "the included project key survives");
            assert!(rendered.contains("claude-code:sess-0"), "the session key survives");
    }
}
