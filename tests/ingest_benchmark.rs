//! A hand-run benchmark for `aub-mh1c`: ingests a generated 1000-file corpus
//! and prints events per second, run before and after the profiled change to
//! record the effect on this bead. `#[ignore]`d because its point is a wall-
//! clock number a human reads, not a pass/fail assertion `bin/ci` should
//! gate on: run it explicitly with
//! `cargo test -p agent-usage-book --test ingest_benchmark -- --ignored --nocapture`.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use agent_usage_book::config::{FakeEnv, Overrides, resolve};
use agent_usage_book::domain::time::RealClock;
use agent_usage_book::ingest::{IngestOptions, run as run_ingest};
use agent_usage_book::store::connection::{AccessMode, PragmaPolicy, open};
use agent_usage_book::store::migrate::run_migrations;

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("aub-mh1c-benchmark-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("scratch dir must be creatable");
    dir
}

/// Writes `files` transcript files of `events_per_file` usage events each, in
/// the shape a real Claude Code transcript carries: one JSON object per line,
/// a distinct session per file, a distinct native message id per event. 20
/// events per file matches the operator's own corpus (about 75000 events
/// over 3831 files, `aub-mh1c`'s own measurement).
fn write_corpus(root: &Path, files: u64, events_per_file: u64) {
    fs::create_dir_all(root).expect("corpus root must be creatable");
    for file in 0..files {
        let mut body = String::new();
        for message in 0..events_per_file {
            body.push_str(&format!(
                r#"{{"type":"assistant","timestamp":"2026-08-25T10:{:02}:00.000Z","sessionId":"s{file}","message":{{"id":"m{file}-{message}","usage":{{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}}}}"#,
                message % 60
            ));
            body.push('\n');
        }
        fs::write(root.join(format!("file{file}.jsonl")), body)
            .expect("corpus file must be writable");
    }
}

fn config_for(corpus: &Path, state_dir: &Path) -> agent_usage_book::config::Config {
    let toml = format!(
        r#"
[state]
dir = "{}"

[[transcripts]]
name = "benchmark-corpus"
root = "{}"
pattern = "*.jsonl"
format = "claude-code"
"#,
        state_dir.display(),
        corpus.display()
    );
    resolve(
        &Overrides::new(),
        &FakeEnv::new(),
        Some(&toml),
        "/virtual/aub.toml",
    )
    .expect("resolve benchmark config")
    .0
}

/// Generates a 1000-file corpus (about 20000 events, the operator's own
/// density) into a scratch directory, ingests it end to end through the real
/// pipeline, and prints wall time and events per second. Run by hand before
/// and after the profiled change (`aub-mh1c`); the numbers this bead records
/// came from this test against the real 3831-file corpus copied into a
/// scratch directory, not this synthetic one, but the shape is the same.
#[test]
#[ignore = "prints a wall-clock number for a human to read; not a pass/fail gate"]
fn benchmark_a_1000_file_corpus_ingest() {
    let root = scratch("corpus");
    let corpus = root.join("corpus");
    let state_dir = root.join("state");
    write_corpus(&corpus, 1_000, 20);
    let config = config_for(&corpus, &state_dir);

    let db_path = state_dir.join("ledger.db");
    fs::create_dir_all(&state_dir).unwrap();
    let mut conn = open(
        &db_path,
        AccessMode::ReadWrite,
        &PragmaPolicy {
            busy_timeout: agent_usage_book::domain::time::MonotonicDuration::from_seconds(30),
        },
    )
    .unwrap();
    run_migrations(
        &mut conn,
        &agent_usage_book::store::migrations::registry(),
        None,
        &RealClock::new(),
    )
    .unwrap();

    let started = Instant::now();
    let report = run_ingest(
        &mut conn,
        &config,
        &IngestOptions::default(),
        &RealClock::new(),
        &mut |_batch| Ok(()),
        &mut |progress| {
            eprintln!(
                "benchmark: files={}/{} events={} rate={:.1}/s",
                progress.files_done,
                progress.files_total,
                progress.events_written,
                progress.rate_events_per_sec
            );
            Ok(())
        },
    )
    .expect("the benchmark ingest must complete");
    let elapsed = started.elapsed();

    let total_events =
        report.outcome.events_written.value() + report.outcome.events_already_ingested.value();
    let events_per_second = total_events as f64 / elapsed.as_secs_f64();
    eprintln!(
        "benchmark: files_parsed={} events={} elapsed={:.2}s events_per_second={:.1}",
        report.files_parsed,
        total_events,
        elapsed.as_secs_f64(),
        events_per_second
    );
    assert_eq!(report.files_parsed, 1_000);
    assert_eq!(total_events, 20_000);
}
