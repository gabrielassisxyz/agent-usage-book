//! aub-4qk2: `aub ingest transcripts` names its phase on every progress line,
//! in order, and labels every rate it prints, while the two stdout summary
//! lines stay exactly what they were. Driven through the real binary, because
//! the progress lines are written by the command's own stderr sink.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "aub-4qk2-ingest-phases-{tag}-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("home")).unwrap();
    fs::create_dir_all(dir.join("state")).unwrap();
    fs::create_dir_all(dir.join("corpus")).unwrap();
    dir
}

fn write_transcript(root: &Path, session: &str) {
    let body = format!(
        r#"{{"type":"assistant","timestamp":"2026-08-25T10:00:00.000Z","sessionId":"{session}","message":{{"id":"m-{session}","usage":{{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}}}}"#,
    );
    fs::write(root.join(format!("{session}.jsonl")), format!("{body}\n")).unwrap();
}

/// One event per batch, so three sessions make three batches and the writing
/// phase has more than its start line to show.
fn config_file(root: &Path) -> PathBuf {
    let path = root.join("aub.toml");
    fs::write(
        &path,
        format!(
            "state.dir = \"{}\"\n\n[ingest]\nmax_batch_events = 1\n\n\
             [[transcripts]]\nname = \"corpus\"\nroot = \"{}\"\npattern = \"*.jsonl\"\nformat = \"claude-code\"\n",
            root.join("state").display(),
            root.join("corpus").display(),
        ),
    )
    .unwrap();
    path
}

fn run_ingest(root: &Path, config: &Path) -> (String, Vec<String>) {
    let output = Command::new(env!("CARGO_BIN_EXE_aub"))
        .env("HOME", root.join("home"))
        .env("AUB_CONFIG_FILE", config)
        .args(["ingest", "transcripts"])
        .output()
        .expect("aub ingest must run");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(output.status.success(), "ingest failed: {stdout}\n{stderr}");
    let progress = stderr
        .lines()
        .filter(|line| line.starts_with("ingest transcripts: "))
        .map(str::to_string)
        .collect();
    (stdout, progress)
}

fn phase(line: &str) -> &str {
    line.trim_start_matches("ingest transcripts: ")
        .split(' ')
        .next()
        .unwrap_or("")
}

#[test]
fn ingest_prints_scanning_then_one_deduplicating_then_writing_to_the_last_batch() {
    let root = scratch("order");
    for session in ["a", "b", "c"] {
        write_transcript(&root.join("corpus"), session);
    }
    let config = config_file(&root);
    let (stdout, progress) = run_ingest(&root, &config);

    let phases: Vec<&str> = progress.iter().map(|line| phase(line)).collect();
    let first_dedup = phases
        .iter()
        .position(|phase| *phase == "deduplicating")
        .unwrap_or_else(|| panic!("no deduplicating line: {progress:#?}"));
    assert!(first_dedup >= 1, "no scanning line first: {progress:#?}");
    assert!(
        phases[..first_dedup]
            .iter()
            .all(|phase| *phase == "scanning"),
        "{progress:#?}"
    );
    assert_eq!(
        phases
            .iter()
            .filter(|phase| **phase == "deduplicating")
            .count(),
        1,
        "{progress:#?}"
    );
    assert!(
        phases[first_dedup + 1..]
            .iter()
            .all(|phase| *phase == "writing"),
        "{progress:#?}"
    );

    let last_scanning = &progress[first_dedup - 1];
    assert!(
        last_scanning.starts_with("ingest transcripts: scanning files=3/3 events=3 elapsed="),
        "{last_scanning}"
    );
    assert!(
        progress[first_dedup].starts_with("ingest transcripts: deduplicating events=3 elapsed="),
        "{}",
        progress[first_dedup]
    );
    let writing_start = &progress[first_dedup + 1];
    assert!(
        writing_start.starts_with("ingest transcripts: writing batches=0/3 events=0/3 elapsed="),
        "{writing_start}"
    );
    assert!(!writing_start.contains("rate="), "{writing_start}");
    let last = progress.last().unwrap();
    assert!(
        last.starts_with("ingest transcripts: writing batches=3/3 events=3/3 elapsed="),
        "{last}"
    );

    for line in &progress {
        assert!(!line.contains("rate=0.0/s"), "{line}");
        assert!(!line.contains("sessions="), "{line}");
        if let Some((_, rate)) = line.split_once(" rate=") {
            let unit = rate.split_once(' ').map(|(_, unit)| unit);
            assert!(
                matches!(unit, Some("files/s") | Some("events/s")),
                "unlabelled rate: {line}"
            );
        }
    }

    // The summary on stdout is outside this change and stays byte-for-byte
    // what it was; each of the three batches advances the generation once.
    assert_eq!(
        stdout,
        "ingest transcripts: sources=corpus scanned=3 parsed=3 skipped=0 unreadable=0 \
         quarantined=0 generation=3 batches=3 working_directory_changes=0\n  \
         events: written=3 already-ingested=0 · occurrences: written=3 already-ingested=0 \
         · components=6 sessions=3 replaced=0\n"
    );
    let _ = fs::remove_dir_all(&root);
}
