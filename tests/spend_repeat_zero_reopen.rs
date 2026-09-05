//! aub-n27.3: a repeat `aub spend` over an unchanged corpus opens zero
//! transcript files. The correctness assertion below reads the exact
//! counter `aub spend --format json` already exposes on its own
//! `ingest.files_read` field (src/cli.rs, `report.ingest.files_read =
//! refresh.files_parsed`), which PLAN.md 17.2's watermark and
//! `ChangeClass::Unchanged` (src/ingest.rs) make possible: an unchanged file
//! is skipped before `std::fs::read_to_string` is ever called. Timing is
//! recorded separately, by design (a fast repeat run is not itself proof
//! that zero files were opened; the counter is).

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;

fn aub() -> Command {
    Command::new(env!("CARGO_BIN_EXE_aub"))
}

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "aub-n27-3-spend-zero-reopen-{tag}-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("home")).unwrap();
    fs::create_dir_all(dir.join("state")).unwrap();
    fs::create_dir_all(dir.join("corpus")).unwrap();
    dir
}

fn write_transcript(root: &std::path::Path, session: &str) {
    let body = format!(
        r#"{{"type":"assistant","timestamp":"2026-08-25T10:00:00.000Z","sessionId":"{session}","message":{{"id":"m-{session}","usage":{{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}}}}"#,
    );
    fs::write(root.join(format!("{session}.jsonl")), format!("{body}\n")).unwrap();
}

fn config_file(root: &std::path::Path) -> PathBuf {
    let path = root.join("aub.toml");
    fs::write(
        &path,
        format!(
            "state.dir = \"{}\"\n\n[[transcripts]]\nname = \"corpus\"\nroot = \"{}\"\npattern = \"*.jsonl\"\nformat = \"claude-code\"\n",
            root.join("state").display(),
            root.join("corpus").display(),
        ),
    )
    .unwrap();
    path
}

fn run_spend(
    root: &std::path::Path,
    config: &std::path::Path,
    extra_args: &[&str],
) -> (i32, serde_json::Value, std::time::Duration) {
    let mut command = aub();
    command
        .env("HOME", root.join("home"))
        .env("AUB_CONFIG_FILE", config)
        .args(["spend", "--format", "json"])
        .args(extra_args);
    let started = Instant::now();
    let output = command.output().expect("aub spend must run");
    let elapsed = started.elapsed();
    let code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let document: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|error| panic!("spend JSON must parse: {error}: {stdout}"));
    (code, document, elapsed)
}

/// The positive case: after an initial pass has recorded a watermark for
/// every file, a second default (`--refresh auto`) pass over the identical
/// corpus opens none of them.
#[test]
fn repeating_spend_over_an_unchanged_corpus_opens_zero_transcript_files() {
    let root = scratch("correctness");
    for session in ["a", "b", "c"] {
        write_transcript(&root.join("corpus"), session);
    }
    let config = config_file(&root);

    let (first_code, first, _first_elapsed) = run_spend(&root, &config, &[]);
    assert_eq!(first_code, 0, "{first}");
    assert_eq!(
        first["ingest"]["files_read"], 3,
        "the first pass must actually read the seeded corpus: {first}"
    );

    let (second_code, second, second_elapsed) = run_spend(&root, &config, &[]);
    assert_eq!(second_code, 0, "{second}");
    assert_eq!(
        second["ingest"]["files_read"], 0,
        "a repeat pass over an unchanged corpus must open zero transcript files: {second}"
    );

    // Timing is recorded, not gated: a wall-clock number does not itself
    // prove "zero files opened", the counter above does.
    eprintln!(
        "aub-n27.3: repeat spend over an unchanged 3-file corpus took {second_elapsed:?} (recorded, not a pass/fail gate)"
    );

    let _ = fs::remove_dir_all(&root);
}

/// The planted negative: without this, an assertion that never moves would
/// pass just as well. Forcing a refresh over the identical corpus must
/// re-read every file, proving `files_read == 0` above reflects the
/// watermark skip and not a counter that stays at zero regardless of what
/// ran.
#[test]
fn forcing_a_refresh_over_the_same_corpus_reads_every_file_again() {
    let root = scratch("negative");
    for session in ["a", "b", "c"] {
        write_transcript(&root.join("corpus"), session);
    }
    let config = config_file(&root);

    let (first_code, first, _) = run_spend(&root, &config, &[]);
    assert_eq!(first_code, 0, "{first}");
    assert_eq!(first["ingest"]["files_read"], 3);

    let (forced_code, forced, _) = run_spend(&root, &config, &["--refresh", "force"]);
    assert_eq!(forced_code, 0, "{forced}");
    assert_eq!(
        forced["ingest"]["files_read"], 3,
        "a forced refresh must re-read every file, not reuse the watermark: {forced}"
    );

    let _ = fs::remove_dir_all(&root);
}
