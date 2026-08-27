//! Integration: discovery over a synthetic tree of stated size completes
//! within its budget, measured against the real wall clock. The unit tests in
//! `src/transcripts/discovery.rs` prove the walk logic; this test proves the
//! real elapsed time stays under the budget, which an injected clock cannot
//! measure. It lives in `tests/` because the system-clock guard only permits
//! `src/domain/time.rs` to read the clock inside `src/`.

use std::path::{Path, PathBuf};
use std::time::Instant;

use agent_usage_book::config::TranscriptConfig;
use agent_usage_book::transcripts::{DiscoveryOptions, discover};

fn config(name: &str, root: &Path, pattern: &str) -> TranscriptConfig {
    TranscriptConfig {
        name: name.to_string(),
        root: root.to_path_buf(),
        pattern: pattern.to_string(),
        usage_evidence: None,
    }
}

/// A scratch tree unique to one test, removed before and after.
fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("aub-lqe1-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(path: &Path, contents: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

/// Discovery over a synthetic tree of stated size completes within its
/// budget: a deep root is the case that would otherwise walk a filesystem.
#[test]
fn discovery_over_a_synthetic_tree_completes_within_budget() {
    let root = scratch("perf");
    for level in 0..20 {
        for branch in 0..5 {
            let dir = root.join(format!("l{level}")).join(format!("b{branch}"));
            write(&dir.join(format!("t-{level}-{branch}.jsonl")), "{}");
        }
    }
    let sources = [config("s", &root, "*.jsonl")];
    let started = Instant::now();
    let result = discover(&sources, &DiscoveryOptions::default()).unwrap();
    let elapsed = started.elapsed();
    assert_eq!(result[0].files.len(), 100, "20 levels x 5 branches");
    assert!(
        elapsed.as_secs() < 5,
        "discovery of 100 files took {elapsed:?}, over the 5s budget"
    );
}
