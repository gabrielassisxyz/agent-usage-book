//! Integration proofs for character-count reconstructed transcript usage.
//!
//! These tests exercise the public ingest, canonical store, and spend-report
//! paths together. Unit tests pin the estimator arithmetic; this file pins the
//! metadata that must survive after the parser event itself is gone.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use agent_usage_book::config::{Config, FakeEnv, ModelTable, Overrides, resolve};
use agent_usage_book::dedup::HeuristicKey;
use agent_usage_book::domain::time::{FakeClock, MonotonicDuration, UtcDate, UtcTimestamp};
use agent_usage_book::evidence::{EstimatorId, EvidenceQuality};
use agent_usage_book::ingest::{IngestOptions, run as run_ingest};
use agent_usage_book::report::spend::{CreditReporting, SpendWindow, assemble_canonical};
use agent_usage_book::store::connection::PragmaPolicy;

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new(tag: &str) -> Self {
        let suffix = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "aub-estimated-transcripts-{tag}-{}-{suffix}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("scratch directory must be creatable");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn migrated_conn(path: &Path) -> rusqlite::Connection {
    test_support::open_migrated(
        path,
        &PragmaPolicy {
            busy_timeout: MonotonicDuration::from_millis(5_000),
        },
    )
}

fn config(sources: &[(&str, &Path, &str)]) -> Config {
    let mut toml = String::new();
    for (name, root, format) in sources {
        toml.push_str(&format!(
            "\n[[transcripts]]\nname = \"{name}\"\nroot = \"{}\"\npattern = \"**/*.jsonl\"\nformat = \"{format}\"\n",
            root.display()
        ));
    }
    resolve(
        &Overrides::new(),
        &FakeEnv::new(),
        Some(&toml),
        "/virtual/aub.toml",
    )
    .expect("test transcript configuration must resolve")
    .0
}

fn ingest(conn: &mut rusqlite::Connection, config: &Config) {
    run_ingest(
        conn,
        config,
        &IngestOptions::default(),
        &FakeClock::new(UtcTimestamp::parse_rfc3339("2026-09-20T12:00:00Z").unwrap()),
        &mut |_| Ok(()),
        &mut |_| Ok(()),
    )
    .expect("fixture transcripts must ingest");
}

fn spend(conn: &rusqlite::Connection) -> agent_usage_book::report::SpendReport {
    assemble_canonical(
        conn,
        SpendWindow::starting(UtcDate::parse("2026-09-20").unwrap(), 1).unwrap(),
        UtcTimestamp::parse_rfc3339("2026-09-20T12:01:00Z").unwrap(),
        Vec::new(),
        false,
        None,
        None,
        CreditReporting::NotRequested,
        &ModelTable::default(),
    )
    .expect("canonical spend report must assemble")
}

fn write_agy_fixture(root: &Path) {
    let logs = root.join("session-a/.system_generated/logs");
    fs::create_dir_all(&logs).unwrap();
    fs::write(
        logs.join("transcript_full.jsonl"),
        concat!(
            "{\"type\":\"USER_INPUT\",\"content\":\"abcdefgh\"}\n",
            "{\"type\":\"PLANNER_RESPONSE\",\"created_at\":\"2026-09-20T10:00:00Z\",\"content\":\"abcdefgh\"}\n"
        ),
    )
    .unwrap();
}

fn write_measured_fixture(root: &Path) {
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("measured.jsonl"),
        "{\"type\":\"assistant\",\"message\":{\"id\":\"measured-1\",\"model\":\"claude-opus-4\",\"usage\":{\"input_tokens\":10,\"output_tokens\":5}},\"timestamp\":\"2026-09-20T10:01:00Z\"}\n",
    )
    .unwrap();
}

#[test]
fn estimator_metadata_and_absent_uncertainty_round_trip_through_the_ledger() {
    let scratch = ScratchDir::new("round-trip");
    let agy_root = scratch.path().join("agy");
    write_agy_fixture(&agy_root);
    let mut conn = migrated_conn(&scratch.path().join("ledger.sqlite3"));
    ingest(&mut conn, &config(&[("agy", &agy_root, "agy")]));

    let stored: (String, String, Option<String>, String) = conn
        .query_row(
            "SELECT e.evidence_kind, o.identity_strength, o.native_event_id, \
                    o.heuristic_algorithm_version \
             FROM usage_event e JOIN usage_occurrence o ON o.event_id = e.id",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(stored.0, "reconstructed:agy-character-count:1");
    assert_eq!(stored.1, "heuristic");
    assert_eq!(stored.2, None);
    assert_eq!(stored.3, HeuristicKey::ALGORITHM_VERSION);

    match spend(&conn).groups[0].usage.quality() {
        EvidenceQuality::Estimated {
            methods,
            uncertainty,
        } => {
            assert_eq!(
                methods,
                &std::collections::BTreeSet::from([EstimatorId::new(
                    "reconstructed:agy-character-count:1",
                )])
            );
            assert_eq!(uncertainty, &None);
        }
        quality @ (EvidenceQuality::Measured | EvidenceQuality::Mixed { .. }) => {
            panic!("stored agy usage must remain estimated, got {quality:?}")
        }
    }
}

#[test]
fn report_combining_measured_and_reconstructed_sources_is_mixed() {
    let scratch = ScratchDir::new("mixed");
    let agy_root = scratch.path().join("agy");
    let measured_root = scratch.path().join("measured");
    write_agy_fixture(&agy_root);
    write_measured_fixture(&measured_root);
    let mut conn = migrated_conn(&scratch.path().join("ledger.sqlite3"));
    ingest(
        &mut conn,
        &config(&[
            ("agy", &agy_root, "agy"),
            ("claude", &measured_root, "claude-code"),
        ]),
    );

    match spend(&conn).groups[0].usage.quality() {
        EvidenceQuality::Mixed {
            methods,
            uncertainty,
        } => {
            assert_eq!(
                methods,
                &std::collections::BTreeSet::from([EstimatorId::new(
                    "reconstructed:agy-character-count:1",
                )])
            );
            assert_eq!(uncertainty, &None);
        }
        quality @ (EvidenceQuality::Measured | EvidenceQuality::Estimated { .. }) => {
            panic!("measured plus reconstructed usage must be mixed, got {quality:?}")
        }
    }
}
