//! Task-kind identity end to end (`aub-eu7.5`): a fixture tracker database is
//! read through the public boundary, ingested raw, rebuilt under a versioned
//! mapping, and read back as the distribution input the historical queries
//! group on.
//!
//! The properties that no single layer can assert on its own: candidates
//! round-trip through the ledger unchanged, a rebuild under an unchanged
//! mapping is a no-op on the persisted identity, a mapping change re-evaluates
//! the same evidence into a different identity while the evidence itself stays
//! put, and the distribution surface exposes resolved, unknown and conflicting
//! states as typed values with no fallback string anywhere.

use agent_usage_book::attribution::{
    TaskDifficulty, TaskIdentityState, TaskKind, TaskKindMapping, TaskKindOrigin, TaskSize,
    TrackerTaskReader,
};
use agent_usage_book::domain::ids::{NativeTaskId, SourceNamespace, TaskId};
use agent_usage_book::domain::time::{FakeClock, MonotonicDuration, UtcTimestamp};
use agent_usage_book::store::connection::{AccessMode, PragmaPolicy, open};
use agent_usage_book::store::migrate::run_migrations;
use agent_usage_book::store::migrations::registry;
use agent_usage_book::store::task_identity::{
    BeadsTaskKindReader, ingest_task_kind_candidates, read_task_identity, rebuild_task_identities,
    task_kind_distribution, task_size_distribution, task_size_distribution_with_filters,
};

/// One scratch state directory per test, removed on drop.
struct ScratchDir(std::path::PathBuf);

impl ScratchDir {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("aub-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("scratch dir must be creatable");
        Self(path)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Opens a state database migrated to the full current registry.
fn state_db(name: &str) -> (ScratchDir, rusqlite::Connection) {
    let scratch = ScratchDir::new(name);
    let mut connection = open(
        &scratch.path().join("state.db"),
        AccessMode::ReadWrite,
        &PragmaPolicy {
            busy_timeout: MonotonicDuration::from_millis(1_000),
        },
    )
    .unwrap();
    run_migrations(
        &mut connection,
        &registry(),
        None,
        &FakeClock::new(UtcTimestamp::from_unix_nanos(0)),
    )
    .unwrap();
    (scratch, connection)
}

/// Builds a tracker database carrying the `issues` and `labels` shape the
/// Beads tracker actually writes, with the task kinds observed in this
/// repository's own tracker plus the two shapes that exercise unknown and
/// conflict outcomes.
fn fixture_tracker() -> rusqlite::Connection {
    let connection = rusqlite::Connection::open_in_memory().unwrap();
    connection
        .execute_batch(
            "CREATE TABLE issues (
                id TEXT PRIMARY KEY,
                issue_type TEXT NOT NULL DEFAULT 'task',
                title TEXT NOT NULL DEFAULT ''
            );
            CREATE TABLE labels (
                issue_id TEXT NOT NULL,
                label TEXT NOT NULL,
                PRIMARY KEY (issue_id, label)
            );
            INSERT INTO issues (id, issue_type) VALUES
                ('aub-1', 'task'),
                ('aub-2', 'bug'),
                ('aub-3', 'epic'),
                ('aub-4', 'docs'),
                ('aub-5', 'question'),
                ('aub-6', 'spike'),
                ('aub-7', 'spike');
            INSERT INTO labels VALUES
                ('aub-6', 'experiment'),
                ('aub-7', 'alpha'),
                ('aub-7', 'beta');",
        )
        .unwrap();
    connection
}

/// A classification fixture with independent valid, missing, unknown and
/// conflicting size/difficulty labels.
fn classification_fixture_tracker() -> rusqlite::Connection {
    let connection = rusqlite::Connection::open_in_memory().unwrap();
    connection
        .execute_batch(
            "CREATE TABLE issues (
                id TEXT PRIMARY KEY,
                issue_type TEXT NOT NULL DEFAULT 'task',
                title TEXT NOT NULL DEFAULT ''
            );
            CREATE TABLE labels (
                issue_id TEXT NOT NULL,
                label TEXT NOT NULL,
                PRIMARY KEY (issue_id, label)
            );
            INSERT INTO issues (id, issue_type) VALUES
                ('aub-size-s', 'task'),
                ('aub-size-m', 'bug'),
                ('aub-size-m-unknown-difficulty', 'task'),
                ('aub-no-labels', 'docs'),
                ('aub-size-conflict', 'task'),
                ('aub-size-unknown', 'task'),
                ('aub-size-xl', 'task');
            INSERT INTO labels VALUES
                ('aub-size-s', 'size:S'),
                ('aub-size-s', 'difficulty:mechanical'),
                ('aub-size-m', 'size:M'),
                ('aub-size-m', 'difficulty:reasoning'),
                ('aub-size-m-unknown-difficulty', 'size:M'),
                ('aub-size-m-unknown-difficulty', 'difficulty:unfamiliar'),
                ('aub-size-conflict', 'size:S'),
                ('aub-size-conflict', 'size:L'),
                ('aub-size-conflict', 'difficulty:critical'),
                ('aub-size-unknown', 'size:medium'),
                ('aub-size-unknown', 'difficulty:mechanical'),
                ('aub-size-xl', 'size:XL'),
                ('aub-size-xl', 'difficulty:critical');",
        )
        .unwrap();
    connection
}

/// A reader over the fixture connection, named so the boundary trait is
/// exercised through its object form too.
struct FixtureTracker<'a> {
    inner: BeadsTaskKindReader<'a>,
}

impl TrackerTaskReader for FixtureTracker<'_> {
    fn read_tasks(
        &self,
    ) -> Result<Vec<agent_usage_book::attribution::TrackerTaskRecord>, agent_usage_book::error::Error>
    {
        self.inner.read_tasks()
    }
}

fn task(source: &str, native: &str) -> TaskId {
    TaskId::new(SourceNamespace::new(source), NativeTaskId::new(native))
}

#[test]
fn candidates_persist_round_trip_and_rebuild_to_the_same_identity() {
    let (_scratch, connection) = state_db("identity-roundtrip");
    let tracker = fixture_tracker();
    let reader = FixtureTracker {
        inner: BeadsTaskKindReader::new(&tracker),
    };

    let first =
        ingest_task_kind_candidates(&connection, SourceNamespace::new("beads-a"), &reader).unwrap();
    // Six tasks, one identity-field candidate each, four label candidates.
    assert_eq!(first.candidates_inserted, 10);

    let second =
        ingest_task_kind_candidates(&connection, SourceNamespace::new("beads-a"), &reader).unwrap();
    assert_eq!(second.candidates_inserted, 0);
    assert_eq!(second.candidates_already_present, 10);

    let mapping = TaskKindMapping::default_v1();
    rebuild_task_identities(&connection, &mapping).unwrap();

    let resolved = read_task_identity(&connection, &task("beads-a", "aub-2"))
        .unwrap()
        .expect("a task with an identity-field candidate");
    assert_eq!(resolved.state, TaskIdentityState::Resolved);
    assert_eq!(resolved.kind, Some(TaskKind::Bug));
    assert_eq!(
        resolved.winner,
        Some(TaskKindOrigin::TrackerField("issue_type".to_owned()))
    );
    assert_eq!(resolved.normalization_version, mapping.version());

    // Same evidence, same mapping: the rebuild is a no-op on the row.
    let before = read_task_identity(&connection, &task("beads-a", "aub-2"))
        .unwrap()
        .unwrap();
    rebuild_task_identities(&connection, &mapping).unwrap();
    let after = read_task_identity(&connection, &task("beads-a", "aub-2"))
        .unwrap()
        .unwrap();
    assert_eq!(before, after);
}

#[test]
fn a_mapping_change_re_evaluates_the_same_evidence() {
    let (_scratch, connection) = state_db("identity-remap");
    let tracker = fixture_tracker();
    let reader = FixtureTracker {
        inner: BeadsTaskKindReader::new(&tracker),
    };
    ingest_task_kind_candidates(&connection, SourceNamespace::new("beads-a"), &reader).unwrap();

    rebuild_task_identities(&connection, &TaskKindMapping::default_v1()).unwrap();
    let spike = read_task_identity(&connection, &task("beads-a", "aub-6"))
        .unwrap()
        .unwrap();
    // "spike" is outside the default vocabulary and its tag is unmapped, so
    // the task stays explicitly unknown with its evidence retained.
    assert_eq!(spike.state, TaskIdentityState::Unknown);
    assert_eq!(spike.kind, None);
    assert!(spike.evidence.contains("tracker_field:issue_type=spike"));

    // A newer mapping covers the tag; the same immutable candidates now
    // resolve, at a newer normalization version, with the evidence unchanged.
    let v2 = TaskKindMapping::new(
        2,
        [
            ("task", TaskKind::Task),
            ("epic", TaskKind::Epic),
            ("bug", TaskKind::Bug),
            ("docs", TaskKind::Docs),
            ("question", TaskKind::Question),
            ("spike", TaskKind::Task),
            ("experiment", TaskKind::Bug),
        ]
        .into_iter()
        .map(|(raw, kind)| (raw.to_owned(), kind)),
    )
    .unwrap();
    rebuild_task_identities(&connection, &v2).unwrap();
    let reinterpreted = read_task_identity(&connection, &task("beads-a", "aub-6"))
        .unwrap()
        .unwrap();
    assert_eq!(reinterpreted.state, TaskIdentityState::Resolved);
    assert_eq!(reinterpreted.kind, Some(TaskKind::Task));
    assert_eq!(reinterpreted.normalization_version, 2);
    assert_eq!(reinterpreted.evidence, spike.evidence);

    // The immutable evidence did not move: re-ingesting under the new mapping
    // changes nothing about the candidate rows.
    let summary =
        ingest_task_kind_candidates(&connection, SourceNamespace::new("beads-a"), &reader).unwrap();
    assert_eq!(summary.candidates_inserted, 0);
    assert_eq!(summary.candidates_already_present, 10);
}

#[test]
fn the_distribution_input_reports_typed_states_and_counts() {
    let (_scratch, connection) = state_db("identity-distribution");
    let tracker = fixture_tracker();
    let reader = FixtureTracker {
        inner: BeadsTaskKindReader::new(&tracker),
    };
    ingest_task_kind_candidates(&connection, SourceNamespace::new("beads-a"), &reader).unwrap();

    // v1 leaves aub-7's two equal-rank tags ("alpha", "beta") uncovered, so
    // nothing asserts a kind for it beyond its unmapped identity field; the
    // conflict in this fixture needs a mapping that maps both tags differently.
    let mapping = TaskKindMapping::new(
        3,
        [
            ("task", TaskKind::Task),
            ("epic", TaskKind::Epic),
            ("bug", TaskKind::Bug),
            ("docs", TaskKind::Docs),
            ("question", TaskKind::Question),
            ("spike", TaskKind::Task),
            ("alpha", TaskKind::Bug),
            ("beta", TaskKind::Docs),
        ]
        .into_iter()
        .map(|(raw, kind)| (raw.to_owned(), kind)),
    )
    .unwrap();
    rebuild_task_identities(&connection, &mapping).unwrap();

    let distribution = task_kind_distribution(&connection).unwrap();
    // Under v3 the identity field of every fixture task maps, so the two
    // "spike" tasks resolve to Task; the alpha/beta tags lose to the rank-1
    // field and nothing is unknown or conflicted.
    assert_eq!(distribution.count_for(TaskKind::Task), 3);
    assert_eq!(distribution.count_for(TaskKind::Bug), 1);
    assert_eq!(distribution.count_for(TaskKind::Epic), 1);
    assert_eq!(distribution.count_for(TaskKind::Docs), 1);
    assert_eq!(distribution.count_for(TaskKind::Question), 1);
    assert_eq!(distribution.unknown, 0);
    assert_eq!(distribution.conflict, 0);

    // The conflict lives on a task whose identity field is unmapped while two
    // equal-rank tags disagree: aub-7 when "spike" stays outside the vocabulary.
    let no_spike = TaskKindMapping::new(
        4,
        [
            ("task", TaskKind::Task),
            ("epic", TaskKind::Epic),
            ("bug", TaskKind::Bug),
            ("docs", TaskKind::Docs),
            ("question", TaskKind::Question),
            ("alpha", TaskKind::Bug),
            ("beta", TaskKind::Docs),
        ]
        .into_iter()
        .map(|(raw, kind)| (raw.to_owned(), kind)),
    )
    .unwrap();
    rebuild_task_identities(&connection, &no_spike).unwrap();
    let distribution = task_kind_distribution(&connection).unwrap();
    assert_eq!(distribution.unknown, 1);
    assert_eq!(distribution.conflict, 1);
    let conflicted = read_task_identity(&connection, &task("beads-a", "aub-7"))
        .unwrap()
        .unwrap();
    assert_eq!(conflicted.state, TaskIdentityState::Conflict);
    assert_eq!(conflicted.kind, None);
    assert_eq!(conflicted.winner, None);
    assert!(conflicted.evidence.contains("tracker_label:alpha=alpha"));
    assert!(conflicted.evidence.contains("tracker_label:beta=beta"));
}

#[test]
fn a_task_the_tracker_never_mentioned_is_absent_not_unknown() {
    let (_scratch, connection) = state_db("identity-absent");
    let tracker = fixture_tracker();
    let reader = FixtureTracker {
        inner: BeadsTaskKindReader::new(&tracker),
    };
    ingest_task_kind_candidates(&connection, SourceNamespace::new("beads-a"), &reader).unwrap();
    rebuild_task_identities(&connection, &TaskKindMapping::default_v1()).unwrap();

    assert!(
        read_task_identity(&connection, &task("beads-a", "aub-never-ingested"))
            .unwrap()
            .is_none()
    );
}

#[test]
fn identity_rows_from_two_tracker_sources_do_not_merge() {
    let (_scratch, connection) = state_db("identity-namespaces");
    let tracker = fixture_tracker();
    let reader = FixtureTracker {
        inner: BeadsTaskKindReader::new(&tracker),
    };
    ingest_task_kind_candidates(&connection, SourceNamespace::new("beads-a"), &reader).unwrap();
    ingest_task_kind_candidates(&connection, SourceNamespace::new("beads-b"), &reader).unwrap();
    rebuild_task_identities(&connection, &TaskKindMapping::default_v1()).unwrap();

    let distribution = task_kind_distribution(&connection).unwrap();
    // Every task exists once per tracker source, so the counts double.
    assert_eq!(distribution.count_for(TaskKind::Task), 2);
    assert_eq!(distribution.count_for(TaskKind::Bug), 2);

    let a = read_task_identity(&connection, &task("beads-a", "aub-2"))
        .unwrap()
        .unwrap();
    let b = read_task_identity(&connection, &task("beads-b", "aub-2"))
        .unwrap()
        .unwrap();
    assert_eq!(a.task_id, task("beads-a", "aub-2"));
    assert_eq!(b.task_id, task("beads-b", "aub-2"));
    assert_ne!(a.task_id, b.task_id);
}

#[test]
fn labelled_axes_persist_rebuild_and_group_by_size_with_difficulty_mix() {
    let (_scratch, connection) = state_db("classification-axes");
    let tracker = classification_fixture_tracker();
    let reader = FixtureTracker {
        inner: BeadsTaskKindReader::new(&tracker),
    };

    let first =
        ingest_task_kind_candidates(&connection, SourceNamespace::new("beads-a"), &reader).unwrap();
    assert_eq!(first.candidates_inserted, 20);
    let candidate_snapshot: Vec<(String, String)> = connection
        .prepare(
            "SELECT origin, raw_value FROM task_kind_candidate
             WHERE task_source = 'beads-a' ORDER BY origin, raw_value",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(
        candidate_snapshot
            .iter()
            .any(|(origin, raw)| origin == "tracker_label:size:medium" && raw == "size:medium")
    );

    rebuild_task_identities(&connection, &TaskKindMapping::default_v1()).unwrap();
    let small = read_task_identity(&connection, &task("beads-a", "aub-size-s"))
        .unwrap()
        .unwrap();
    assert_eq!(small.size_state, TaskIdentityState::Resolved);
    assert_eq!(small.size, Some(TaskSize::S));
    assert_eq!(small.difficulty_state, TaskIdentityState::Resolved);
    assert_eq!(small.difficulty, Some(TaskDifficulty::Mechanical));

    let no_labels = read_task_identity(&connection, &task("beads-a", "aub-no-labels"))
        .unwrap()
        .unwrap();
    assert_eq!(no_labels.size_state, TaskIdentityState::Unknown);
    assert_eq!(no_labels.difficulty_state, TaskIdentityState::Unknown);

    let conflict = read_task_identity(&connection, &task("beads-a", "aub-size-conflict"))
        .unwrap()
        .unwrap();
    assert_eq!(conflict.size_state, TaskIdentityState::Conflict);
    assert_eq!(conflict.size, None);
    assert!(conflict.size_evidence.contains("size:S"));
    assert!(conflict.size_evidence.contains("size:L"));

    let unknown = read_task_identity(&connection, &task("beads-a", "aub-size-unknown"))
        .unwrap()
        .unwrap();
    assert_eq!(unknown.size_state, TaskIdentityState::Unknown);
    assert!(unknown.size_evidence.contains("size:medium"));

    let distribution = task_kind_distribution(&connection).unwrap();
    assert_eq!(distribution.count_for_size(TaskSize::S), 1);
    assert_eq!(distribution.count_for_size(TaskSize::M), 2);
    assert_eq!(distribution.count_for_size(TaskSize::XL), 1);
    assert_eq!(distribution.unknown_size, 2);
    assert_eq!(distribution.conflict_size, 1);
    let medium = distribution.group_for_size(TaskSize::M).unwrap();
    assert_eq!(medium.sample_count, 2);
    assert_eq!(
        medium.difficulty_mix.count_for(TaskDifficulty::Reasoning),
        1
    );
    assert_eq!(medium.difficulty_mix.unknown, 1);
    assert_eq!(
        distribution
            .group_for_size(TaskSize::S)
            .unwrap()
            .difficulty_mix
            .count_for(TaskDifficulty::Mechanical),
        1
    );
    assert_eq!(distribution.exclusions().unknown_size, 2);
    assert_eq!(distribution.exclusions().conflict_size, 1);

    let bug_distribution = task_size_distribution(&connection, Some(TaskKind::Bug)).unwrap();
    assert_eq!(bug_distribution.count_for_size(TaskSize::M), 1);
    assert_eq!(bug_distribution.count_for_size(TaskSize::S), 0);
    let reasoning_distribution =
        task_size_distribution_with_filters(&connection, None, Some(TaskDifficulty::Reasoning))
            .unwrap();
    assert_eq!(reasoning_distribution.count_for_size(TaskSize::M), 1);
    assert_eq!(reasoning_distribution.count_for_size(TaskSize::S), 0);

    let revised = TaskKindMapping::default_v1()
        .with_size_entry(2, "size:medium".to_owned(), TaskSize::M)
        .unwrap()
        .with_difficulty_entry(
            2,
            "difficulty:unfamiliar".to_owned(),
            TaskDifficulty::Reasoning,
        )
        .unwrap();
    rebuild_task_identities(&connection, &revised).unwrap();
    let reinterpreted = read_task_identity(&connection, &task("beads-a", "aub-size-unknown"))
        .unwrap()
        .unwrap();
    assert_eq!(reinterpreted.size_state, TaskIdentityState::Resolved);
    assert_eq!(reinterpreted.size, Some(TaskSize::M));
    assert_eq!(reinterpreted.difficulty, Some(TaskDifficulty::Mechanical));
    assert_eq!(reinterpreted.normalization_version, 2);
    assert!(reinterpreted.size_evidence.contains("size:medium"));
    let difficulty_reinterpreted = read_task_identity(
        &connection,
        &task("beads-a", "aub-size-m-unknown-difficulty"),
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        difficulty_reinterpreted.difficulty,
        Some(TaskDifficulty::Reasoning)
    );

    let candidate_snapshot_after: Vec<(String, String)> = connection
        .prepare(
            "SELECT origin, raw_value FROM task_kind_candidate
             WHERE task_source = 'beads-a' ORDER BY origin, raw_value",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(candidate_snapshot, candidate_snapshot_after);
}

#[test]
fn the_persisted_identity_refuses_state_labels_it_cannot_parse() {
    let (_scratch, connection) = state_db("identity-refusal");
    // The database is the first line of defense: a row carrying a state
    // label this code does not know cannot be stored at all, so a newer
    // writer cannot hand an unparseable row to this version through the
    // table's own constraint. The CHECK is what makes the read path's parse
    // refusal reachable only under schema drift, which is why the refusal
    // contract is asserted at both reachable lines.
    let error = connection
        .execute(
            "INSERT INTO task_identity (
                task_source, task_native, state, kind, winner_origin,
                evidence, normalization_version
            ) VALUES ('beads-a', 'aub-1', 'postponed', NULL, NULL, 'tracker_field:issue_type=task', 1)",
            [],
        )
        .unwrap_err();
    assert!(
        error.to_string().contains("CHECK"),
        "the database must refuse an unknown state label: {error}"
    );
    // The typed refusal is the second line: the state vocabulary parses
    // exactly its own labels, so a label that slips past the schema (a
    // future migration relaxing the check, read by older code) is a store
    // failure naming the value, never a silent fallback.
    assert_eq!(TaskIdentityState::parse("postponed"), None);
    assert_eq!(
        TaskIdentityState::parse("resolved"),
        Some(TaskIdentityState::Resolved)
    );
}

#[test]
fn the_persisted_identity_refuses_a_resolved_row_without_its_winner() {
    let (_scratch, connection) = state_db("identity-checks");
    let error = connection
        .execute(
            "INSERT INTO task_identity (
                task_source, task_native, state, kind, winner_origin,
                evidence, normalization_version
            ) VALUES ('beads-a', 'aub-1', 'resolved', 'bug', NULL, 'tracker_field:issue_type=bug', 1)",
            [],
        )
        .unwrap_err();
    assert!(
        error.to_string().contains("CHECK"),
        "the database must refuse a resolved row without a winner: {error}"
    );
    let error = connection
        .execute(
            "INSERT INTO task_identity (
                task_source, task_native, state, kind, winner_origin,
                evidence, normalization_version
            ) VALUES ('beads-a', 'aub-1', 'unknown', 'bug', 'tracker_field:issue_type=bug', 'tracker_field:issue_type=bug', 1)",
            [],
        )
        .unwrap_err();
    assert!(
        error.to_string().contains("CHECK"),
        "the database must refuse an unknown row that carries a kind: {error}"
    );
}
