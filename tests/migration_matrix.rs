//! The migration matrix (`aub-n27.1`): every migration applied forward from
//! every prior schema version, over a populated fixture database.
//!
//! Migrations are forward-only (PLAN.md section 11.4), so a broken one is
//! discovered by the operator it breaks unless CI finds it first. This module
//! is that CI gate. One matrix row per schema version `s` from 0 through the
//! registry's highest: build a database at `s`, fill every table that exists
//! at `s` with rows, then apply migrations `s+1..=N` one at a time. Migration
//! `m` is therefore exercised from every version that precedes it, which is
//! the full upgrade matrix, not a single happy path.
//!
//! Populated fixtures are the part that matters. A migration over an empty
//! database exercises the DDL and nothing else, while the failures that occur
//! in the field involve data: a constraint existing rows violate, a rewrite
//! that loses a column's meaning, a foreign key that finds an orphan nobody
//! knew about. The population step inserts edge values at constraint
//! boundaries (equalities, zeros, enum extremes, a maximal generation
//! counter) so the boundary is exercised, not merely approached.
//!
//! The module extends automatically as migrations land. The rows come from
//! `registry()` (one row per version, nothing hardcoded), and the population
//! step discovers user tables from SQLite and refuses any table it has no
//! entry for. Landing a migration that adds a table therefore fails the suite
//! until the matrix is taught what a row of that table looks like, which is
//! the structural gate the bead asks for: a migration without a matrix entry
//! is a red suite rather than a silent skip. A migration file added without a
//! registry entry is caught the same way, by the file-to-registry check.
//!
//! The verified-backup precondition is part of the matrix. Before a migration
//! marked `rewrites_irreplaceable` applies, the row captures a SQLite backup
//! of the fixture and verifies it with the production verifier (`store::
//! backup::verify_database`, integrity plus foreign keys), because an
//! unverified archive is not yet a backup (PLAN.md section 38). The rewrite
//! scenarios prove both directions over populated fixtures: the migration
//! proceeds with a verified backup at the version in force and refuses
//! without one, including when the verified backup predates that version.
//!
//! CI time budget: the full matrix (19 rows, 171 single-migration
//! applications at 18 migrations) is fsync-bound. Every migration step and
//! every fixture build commits under the production policy (`synchronous =
//! FULL`), and the measured baseline on the development machine is one to
//! four minutes depending on how loaded the disk is (110 s and 220 s across
//! two runs at load average 13 to 16 on 16 cores). The budget is therefore
//! 10 minutes: about three times the worst measured run, and tight enough to
//! catch the regressions the gate exists for (a population entry that
//! inserts thousands of rows, a fixture rebuilt per application, an
//! accidental quadratic), which would blow past it by orders of magnitude.
//! The budget is not a claim about machine speed and is asserted by the
//! matrix test itself.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use agent_usage_book::domain::time::{FakeClock, MonotonicDuration, UtcTimestamp};
use agent_usage_book::error::Error;
use agent_usage_book::store::backup;
use agent_usage_book::store::connection::{AccessMode, PragmaPolicy, open};
use agent_usage_book::store::migrate::{
    Migration, MigrationSummary, VerifiedBackup, current_schema_version, run_migrations,
};
use agent_usage_book::store::migrations::registry;

/// The documented CI time budget for the full matrix. The measured baseline
/// is one to four minutes for 19 rows, dominated by fsyncs under the
/// production durability policy; the budget sits at roughly three times the
/// worst measured run, wide enough for runner variance and tight enough that
/// a population or fixture-build regression cannot hide inside it.
const MATRIX_CI_TIME_BUDGET: Duration = Duration::from_secs(600);

/// The busy timeout for every connection this module opens. The matrix is not
/// contending with anyone, but a finite wait is the connection policy, not a
/// decoration.
const BUSY_TIMEOUT_MILLIS: u64 = 5_000;

fn policy() -> PragmaPolicy {
    PragmaPolicy {
        busy_timeout: MonotonicDuration::from_millis(BUSY_TIMEOUT_MILLIS),
    }
}

/// The injected clock every migration run records its rows with.
fn clock() -> FakeClock {
    FakeClock::new(UtcTimestamp::from_unix_nanos(0))
}

// ---------------------------------------------------------------------------
// Fixture scaffolding
// ---------------------------------------------------------------------------

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A scratch database path under the system temp dir, removed with its WAL
/// sidecars and its backup copy on drop. A file database rather than
/// `:memory:` because the connection policy requires WAL, which an in-memory
/// database cannot report.
struct FixtureDb {
    path: PathBuf,
}

impl FixtureDb {
    fn new(tag: &str) -> Self {
        let count = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "aub-matrix-{tag}-{}-{count}.sqlite3",
            std::process::id()
        ));
        Self { path }
    }

    /// Where this fixture's verified backup archive is written. One path per
    /// fixture: each capture overwrites the previous archive, which is fine
    /// because only the rewrite's own capture is ever consulted.
    fn backup_path(&self) -> PathBuf {
        let mut name = self.path.file_name().unwrap().to_owned();
        name.push(".backup");
        self.path.with_file_name(name)
    }
}

impl Drop for FixtureDb {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm", ".backup", ".backup-wal", ".backup-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", self.path.display()));
        }
    }
}

/// The user tables present right now, read from SQLite rather than from a
/// list, so a table a future migration adds is discovered rather than
/// remembered. The migration framework's own bookkeeping table is excluded:
/// its rows are produced by `run_migrations` and audited there.
fn user_tables(conn: &rusqlite::Connection) -> Vec<String> {
    conn.prepare(
        "SELECT name FROM sqlite_master \
         WHERE type = 'table' AND name NOT LIKE 'sqlite_%' AND name != 'schema_migration'",
    )
    .expect("sqlite_master must be readable")
    .query_map([], |row| row.get::<_, String>(0))
    .expect("table enumeration must run")
    .collect::<Result<Vec<_>, _>>()
    .expect("table names must read")
}

fn table_exists(conn: &rusqlite::Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [name],
        |row| row.get::<_, i64>(0),
    )
    .map(|count| count == 1)
    .unwrap_or(false)
}

fn row_count(conn: &rusqlite::Connection, table: &str) -> Result<i64, String> {
    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
        row.get(0)
    })
    .map_err(|e| format!("cannot count rows in {table}: {e}"))
}

/// One (table, row count) reading per user table, in table order.
fn snapshot_row_counts(
    conn: &rusqlite::Connection,
    tables: &[String],
) -> Result<Vec<(String, i64)>, String> {
    tables
        .iter()
        .map(|table| row_count(conn, table).map(|count| (table.clone(), count)))
        .collect()
}

/// The connection policy enforces foreign keys per connection; reading it
/// back here is what makes the matrix's foreign-key check mean something.
fn assert_foreign_keys_enforced(conn: &rusqlite::Connection) -> Result<(), String> {
    let value: i64 = conn
        .pragma_query_value(None, "foreign_keys", |row| row.get(0))
        .map_err(|e| format!("cannot read PRAGMA foreign_keys: {e}"))?;
    if value != 1 {
        return Err(format!(
            "PRAGMA foreign_keys reports {value}; the matrix only means anything with enforcement on"
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Fixture population: one entry per user table, edge values at boundaries
// ---------------------------------------------------------------------------

/// A population entry: fills one table with rows, including edge values at
/// constraint boundaries. Returns an error naming the table when SQLite
/// refuses a row.
type Populate = fn(&rusqlite::Connection) -> Result<(), String>;

/// Runs one batch of fixture SQL, naming the table in the failure.
fn exec(conn: &rusqlite::Connection, label: &str, sql: &str) -> Result<(), String> {
    conn.execute_batch(sql)
        .map_err(|e| format!("fixture rows for {label}: {e}"))
}

/// The population registry, ordered so parents fill before the children that
/// reference them (foreign keys are enforced on every connection). One entry
/// per user table; the matrix refuses any table this map does not cover, so a
/// future migration's new table fails the suite until its entry lands here.
const POPULATION: &[(&str, Populate)] = &[
    ("account", populate_account),
    ("sample_run", populate_sample_run),
    (
        "sampling_policy_snapshot",
        populate_sampling_policy_snapshot,
    ),
    ("sampling_lease", populate_sampling_lease),
    ("ledger_generation", populate_ledger_generation),
    ("usage_event", populate_usage_event),
    ("usage_component", populate_usage_component),
    ("usage_occurrence", populate_usage_occurrence),
    ("transcript_file", populate_transcript_file),
    ("session", populate_session),
    ("session_account_marker", populate_session_account_marker),
    ("task_event", populate_task_event),
    ("task_event_quarantine", populate_task_event_quarantine),
    ("meter_attempt", populate_meter_attempt),
    ("meter_attempt_result", populate_meter_attempt_result),
    ("task_kind_candidate", populate_task_kind_candidate),
    ("task_identity", populate_task_identity),
    ("cost_model", populate_cost_model),
    ("cost_model_term", populate_cost_model_term),
    ("cost_model_lifecycle", populate_cost_model_lifecycle),
    ("rate_card", populate_rate_card),
    ("meter_response_evidence", populate_meter_response_evidence),
    ("meter_observation", populate_meter_observation),
    ("meter_window", populate_meter_window),
    (
        "meter_observation_preference",
        populate_meter_observation_preference,
    ),
    ("calibration_experiment", populate_calibration_experiment),
    (
        "window_calibration_candidate",
        populate_window_calibration_candidate,
    ),
    (
        "window_calibration_result",
        populate_window_calibration_result,
    ),
    (
        "window_calibration_source_experiment",
        populate_window_calibration_source_experiment,
    ),
    ("calibration_lifecycle", populate_calibration_lifecycle),
    ("attribution_segment", populate_attribution_segment),
    ("ingest_quarantine", populate_ingest_quarantine),
    ("ingestion_generation", populate_ingestion_generation),
    ("legacy_meter_import", populate_legacy_meter_import),
    (
        "legacy_meter_import_record",
        populate_legacy_meter_import_record,
    ),
];

fn populate_account(conn: &rusqlite::Connection) -> Result<(), String> {
    // Row 2 sits on the CHECK boundary: last observed equal to first.
    exec(
        conn,
        "account",
        "INSERT INTO account (id, logical_name, provider_key, first_observed_at, last_observed_at) VALUES
            (1, 'alpha', 'anthropic', 100, 100),
            (2, 'beta', 'anthropic', 0, 1000)",
    )
}

fn populate_sample_run(conn: &rusqlite::Connection) -> Result<(), String> {
    // Row 1 sits on the boundary: ended exactly at started. Row 2 is the
    // still-running case, whose end is NULL.
    exec(
        conn,
        "sample_run",
        "INSERT INTO sample_run (id, trigger, started_at, ended_at, aub_version, configuration_fingerprint) VALUES
            (1, 'manual', 10, 10, '0.0.0-matrix', 'matrix-fingerprint-alpha'),
            (2, 'timer', 20, NULL, '0.0.0-matrix', 'matrix-fingerprint-beta')",
    )
}

fn populate_sampling_policy_snapshot(conn: &rusqlite::Connection) -> Result<(), String> {
    // Row 2 carries zero-length durations: the floor of every duration column.
    exec(
        conn,
        "sampling_policy_snapshot",
        "INSERT INTO sampling_policy_snapshot (id, account_id, effective_at, ordinary_cadence_nanos, freshness_horizon_nanos, reset_edge_policy, retry_backoff_policy, command_budget_nanos, policy_algorithm_version) VALUES
            (1, 1, 100, 3600000000000, 86400000000000, 'wait_for_reset', 'exponential', 300000000000, 'matrix-policy-v1'),
            (2, 2, 200, 0, 0, 'wait_for_reset', 'exponential', 0, 'matrix-policy-v1')",
    )
}

fn populate_sampling_lease(conn: &rusqlite::Connection) -> Result<(), String> {
    // The CHECK is strict: expires strictly after acquired, so the smallest
    // legal gap of one nanosecond is the boundary row.
    exec(
        conn,
        "sampling_lease",
        "INSERT INTO sampling_lease (account_name, holder, acquired_at, expires_at) VALUES
            ('alpha', 'matrix-holder', 0, 1)",
    )
}

fn populate_ledger_generation(conn: &rusqlite::Connection) -> Result<(), String> {
    // The migration seeds one row at zero; the fixture pushes the counter to
    // the INTEGER ceiling so migrations must survive the extreme value.
    exec(
        conn,
        "ledger_generation",
        "UPDATE ledger_generation SET generation = 9223372036854775807 WHERE id = 1",
    )
}

fn populate_usage_event(conn: &rusqlite::Connection) -> Result<(), String> {
    // Row 1 leaves every nullable column null: the all-absent edge.
    exec(
        conn,
        "usage_event",
        "INSERT INTO usage_event (id, canonical_event_id, session_id, event_timestamp, model_id, evidence_kind, source_provenance, parser_version, created_at) VALUES
            (1, 'matrix-canonical-1', NULL, NULL, NULL, 'transcript', 'parser', 'matrix-parser-v1', 600),
            (2, 'matrix-canonical-2', 'matrix-session', 700, 'model-x', 'transcript', 'parser', 'matrix-parser-v1', 601)",
    )
}

fn populate_usage_component(conn: &rusqlite::Connection) -> Result<(), String> {
    // Row 1 carries a zero count: the CHECK floor.
    exec(
        conn,
        "usage_component",
        "INSERT INTO usage_component (id, event_id, token_class, count) VALUES
            (1, 1, 'input', 0),
            (2, 1, 'output', 42),
            (3, 2, 'cache_read', 7)",
    )
}

fn populate_usage_occurrence(conn: &rusqlite::Connection) -> Result<(), String> {
    // Row 1 identifies by native id, row 2 by heuristic key: the two arms of
    // the identity CHECK. A null native id does not collide inside the UNIQUE
    // constraint, which is itself a boundary being exercised.
    exec(
        conn,
        "usage_occurrence",
        "INSERT INTO usage_occurrence (id, source_namespace, native_event_id, parser_version, heuristic_key, source_file, occurred_at) VALUES
            (1, 'transcripts', 'matrix-native-1', 'matrix-parser-v1', NULL, 'rel/alpha.jsonl', 500),
            (2, 'transcripts', NULL, 'matrix-parser-v1', 'matrix-heuristic-1', 'rel/alpha.jsonl', NULL)",
    )?;
    if table_exists(conn, "usage_event") {
        // Version 12 or later: link the occurrence to its event and set the
        // non-default identity strength, exercising the added column's CHECK.
        exec(
            conn,
            "usage_occurrence",
            "UPDATE usage_occurrence SET event_id = 1, identity_strength = 'heuristic' WHERE id = 1",
        )?;
    }
    Ok(())
}

fn populate_transcript_file(conn: &rusqlite::Connection) -> Result<(), String> {
    // Row 1 sits on both floors: size zero, consumed offset zero.
    exec(
        conn,
        "transcript_file",
        "INSERT INTO transcript_file (source_key, relative_path, size, mtime_nanos, identity, parser_version, consumed_offset) VALUES
            ('matrix-source', 'rel/alpha.jsonl', 0, 1, 'matrix-identity-1', 'matrix-parser-v1', 0),
            ('matrix-source', 'rel/beta.jsonl', 4096, 2, 'matrix-identity-2', 'matrix-parser-v1', 4096)",
    )
}

fn populate_session(conn: &rusqlite::Connection) -> Result<(), String> {
    // Row 2 sits on the boundary: end exactly equal to start. Row 1 is the
    // still-open case.
    exec(
        conn,
        "session",
        "INSERT INTO session (id, source, native_session_id, start, end, project_key, repository_key, run_id) VALUES
            (1, 'claude-code', 'matrix-native-1', 100, NULL, 'matrix-project', 'matrix-repo', NULL),
            (2, 'claude-code', 'matrix-native-2', 100, 100, 'matrix-project', 'matrix-repo', 'matrix-run-1')",
    )
}

fn populate_session_account_marker(conn: &rusqlite::Connection) -> Result<(), String> {
    // Row 1 resolves to a real account, row 2 leaves the foreign key null:
    // both arms of the optional reference.
    exec(
        conn,
        "session_account_marker",
        "INSERT INTO session_account_marker (id, session_source, session_native, observed_at, logical_account, resolved_account_id, marker_source, evidence_designation) VALUES
            (1, 'claude-code', 'matrix-native-1', 150, 'alpha', 1, 'transcript', 'direct'),
            (2, 'claude-code', 'matrix-native-2', 250, 'unlinked', NULL, 'inference', 'heuristic')",
    )
}

fn populate_task_event(conn: &rusqlite::Connection) -> Result<(), String> {
    exec(
        conn,
        "task_event",
        "INSERT INTO task_event (id, tracker_source, tracker_event_id, task_source, task_native, occurred_at, event_kind, agent_association) VALUES
            (1, 'beads', 1, 'github', 'matrix-issue-7', 300, 'created', NULL),
            (2, 'beads', 2, 'github', 'matrix-issue-8', 301, 'updated', 'matrix-agent')",
    )
}

fn populate_task_event_quarantine(conn: &rusqlite::Connection) -> Result<(), String> {
    exec(
        conn,
        "task_event_quarantine",
        "INSERT INTO task_event_quarantine (id, tracker_source, tracker_event_id, raw_timestamp, reason) VALUES
            (1, 'beads', 1, 'not-a-timestamp', 'unparseable')",
    )
}

fn populate_meter_attempt(conn: &rusqlite::Connection) -> Result<(), String> {
    // Row 2 sets the due-basis attempt reference (one arm of the exclusive-or
    // CHECK); rows 1 and 3 leave both basis columns null. Three distinct
    // due_reason enum values are covered.
    exec(
        conn,
        "meter_attempt",
        "INSERT INTO meter_attempt (id, run_id, account_id, provider, request_started_at, credential_context_id, policy_snapshot_id, due_at, due_reason, due_basis_attempt_id, due_basis_result_id, provider_contract_id, meter_semantics_id) VALUES
            (1, 1, 1, 'anthropic', 400, NULL, 1, 500, 'ordinary_cadence', NULL, NULL, 'matrix-contract', 'matrix-semantics'),
            (2, 1, 1, 'anthropic', 450, 'matrix-credential', 1, 550, 'forced_or_manual', 1, NULL, 'matrix-contract', 'matrix-semantics'),
            (3, 1, 1, 'anthropic', 500, NULL, 1, 600, 'reset_edge', NULL, NULL, 'matrix-contract', 'matrix-semantics')",
    )
}

fn populate_meter_attempt_result(conn: &rusqlite::Connection) -> Result<(), String> {
    // Row 1 completes exactly at its start (the insert trigger's boundary)
    // with a zero elapsed time. Row 2 is the unreachable outcome with its
    // paired failure class, retry-after and a zero retry index. Row 3
    // completes BEFORE its start, which only the explicit clock-anomaly
    // marker permits.
    exec(
        conn,
        "meter_attempt_result",
        "INSERT INTO meter_attempt_result (attempt_id, completed_at, elapsed_nanos, outcome, failure_class, retry_after_nanos, sanitized_error_classification, retry_index, clock_anomaly) VALUES
            (1, 400, 0, 'success', NULL, NULL, NULL, NULL, 0),
            (2, 600, 150000000, 'unreachable', 'rate_limited', 1000, 'rate_limited', 0, 0),
            (3, 400, 0, 'auth_required', NULL, NULL, NULL, NULL, 1)",
    )
}

fn populate_task_kind_candidate(conn: &rusqlite::Connection) -> Result<(), String> {
    exec(
        conn,
        "task_kind_candidate",
        "INSERT INTO task_kind_candidate (task_source, task_native, origin, raw_value) VALUES
            ('github', 'matrix-issue-7', 'explicit', 'bug')",
    )
}

fn populate_task_identity(conn: &rusqlite::Connection) -> Result<(), String> {
    // One row per state: resolved carries its kind and winner, unknown and
    // conflict must leave both null (the paired CHECKs).
    exec(
        conn,
        "task_identity",
        "INSERT INTO task_identity (task_source, task_native, state, kind, winner_origin, evidence, normalization_version) VALUES
            ('github', 'matrix-issue-7', 'resolved', 'bug', 'explicit', 'matrix-evidence-1', 1),
            ('github', 'matrix-issue-8', 'unknown', NULL, NULL, 'matrix-evidence-2', 1),
            ('github', 'matrix-issue-9', 'conflict', NULL, NULL, 'matrix-evidence-3', 1)",
    )
}

fn populate_cost_model(conn: &rusqlite::Connection) -> Result<(), String> {
    // Row 1 is model-scoped with the plan scope absent; row 2 is class-scoped
    // with the model id absent. Row 1's provenance digest is exactly 16
    // characters (the CHECK requires that length) and its input count is
    // zero (the floor).
    exec(
        conn,
        "cost_model",
        "INSERT INTO cost_model (id, cost_model_id, provider, scope_kind, model_id, billing_semantics_id, plan_scope, version, valid_from, valid_until, published_at, provenance_digest, provenance_input_count) VALUES
            (1, 'matrix-cm-1', 'anthropic', 'model', 'model-x', 'matrix-billing', NULL, 'v1', 0, 1000, 0, '0123456789abcdef', 0),
            (2, 'matrix-cm-2', 'anthropic', 'model_class', NULL, 'matrix-billing', 'pro', 'v1', 0, 1000, 0, 'fedcba9876543210', 3)",
    )
}

fn populate_cost_model_term(conn: &rusqlite::Connection) -> Result<(), String> {
    // Row 1 leaves both uncertainty bounds absent (one arm of the paired
    // CHECK); row 2 makes them equal (the other CHECK's boundary).
    exec(
        conn,
        "cost_model_term",
        "INSERT INTO cost_model_term (id, cost_model_id, token_kind, credits_per_token_micros, uncertainty_low_micros, uncertainty_high_micros, derivation_method, evidence_experiment) VALUES
            (1, 1, 'input', 3, NULL, NULL, 'observed', NULL),
            (2, 1, 'output', 15, 10, 10, 'observed', NULL),
            (3, 2, 'cache_read', 1, NULL, NULL, 'estimated', 'matrix-experiment')",
    )
}

fn populate_cost_model_lifecycle(conn: &rusqlite::Connection) -> Result<(), String> {
    // Activation carries no supersession reference; supersession must.
    exec(
        conn,
        "cost_model_lifecycle",
        "INSERT INTO cost_model_lifecycle (id, cost_model_id, event_kind, event_at, supersedes_model_id) VALUES
            (1, 1, 'activation', 10, NULL),
            (2, 2, 'supersession', 20, 1)",
    )
}

fn populate_rate_card(conn: &rusqlite::Connection) -> Result<(), String> {
    // Row 1 sits on the rate floor of zero with every optional column absent;
    // row 2 fills them and closes the effective window.
    exec(
        conn,
        "rate_card",
        "INSERT INTO rate_card (id, vendor, model, token_class, rate_micros, currency, billing_basis, effective_start, effective_end, imported_at, published_at, source, review_due) VALUES
            (1, 'anthropic', 'model-x', 'input', 0, 'usd', 'per_million_tokens', '2026-01-01', NULL, 5, NULL, NULL, NULL),
            (2, 'anthropic', 'model-x', 'output', 15, 'usd', 'per_million_tokens', '2026-01-01', '2026-12-31', 5, 4, 'matrix-source', '2026-06-01')",
    )
}

fn populate_meter_response_evidence(conn: &rusqlite::Connection) -> Result<(), String> {
    // Both capture-truncated values, and both arms of the optional
    // provider-observed timestamp.
    exec(
        conn,
        "meter_response_evidence",
        "INSERT INTO meter_response_evidence (id, attempt_id, response_classification, received_at, provider_observed_at_original, evidence_capsule, capsule_schema_version, sanitizer_version, content_hash, capture_truncated) VALUES
            (1, 1, 'success', 410, NULL, 'matrix-capsule-1', 'matrix-csv-1', 'matrix-san-1', 'matrix-hash-1', 0),
            (2, 2, 'rate_limited', 610, '2026-01-01T00:00:00Z', 'matrix-capsule-2', 'matrix-csv-1', 'matrix-san-1', 'matrix-hash-2', 1)",
    )
}

fn populate_meter_observation(conn: &rusqlite::Connection) -> Result<(), String> {
    // Row 1 leaves the provider-observed timestamp and the observed plan and
    // tier absent; row 2 fills them under the composite measurement basis.
    exec(
        conn,
        "meter_observation",
        "INSERT INTO meter_observation (id, attempt_id, evidence_id, account_id, provider, provider_observed_at, received_at, measurement_basis, observed_plan, observed_tier, adapter_version, provider_contract_id, meter_semantics_id, normalized_fingerprint) VALUES
            (1, 1, 1, 1, 'anthropic', NULL, 420, 'provider_observed', NULL, NULL, 'matrix-adapter-1', 'matrix-contract', 'matrix-semantics', 'matrix-fingerprint-1'),
            (2, 2, 2, 1, 'anthropic', 620, 630, 'older_of_the_two', 'pro-max', 'tier-1', 'matrix-adapter-1', 'matrix-contract', 'matrix-semantics', 'matrix-fingerprint-2')",
    )
}

fn populate_meter_window(conn: &rusqlite::Connection) -> Result<(), String> {
    // Row 1 sits on both quota-fraction floors: used exactly zero, resolution
    // exactly one. Row 2 sits on both ceilings: used and resolution exactly
    // one million, model-scoped with the model named (both arms of the
    // scope-pairing CHECK).
    exec(
        conn,
        "meter_window",
        "INSERT INTO meter_window (id, observation_id, semantic_key, scope_kind, scoped_model, quota_used_ppm, reported_resolution_ppm, quantization, resets_at, nominal_duration_nanos) VALUES
            (1, 1, 'matrix-key-1', 'account_wide', NULL, 0, 1, 'exact', 1000000000000, 3600000000000),
            (2, 2, 'matrix-key-1', 'model_specific', 'model-x', 1000000, 1000000, 'rounded_down', 2000000000000, 3600000000000)",
    )
}

fn populate_meter_observation_preference(conn: &rusqlite::Connection) -> Result<(), String> {
    exec(
        conn,
        "meter_observation_preference",
        "INSERT INTO meter_observation_preference (evidence_id, meter_semantics_id, current_observation_id) VALUES
            (1, 'matrix-semantics', 1),
            (2, 'matrix-semantics', 2)",
    )
}

fn populate_calibration_experiment(conn: &rusqlite::Connection) -> Result<(), String> {
    // valid_until equal to valid_from: the CHECK boundary.
    exec(
        conn,
        "calibration_experiment",
        "INSERT INTO calibration_experiment (id, experiment_id, provider, plan_tier, window_semantic_key, meter_semantics_id, billing_semantics_id, valid_from, valid_until, knowledge_time) VALUES
            (1, 'matrix-experiment-1', 'anthropic', 'pro', 'matrix-key-1', 'matrix-semantics', 'matrix-billing', 0, 0, 10)",
    )
}

fn populate_window_calibration_candidate(conn: &rusqlite::Connection) -> Result<(), String> {
    // Zero fit residual, equal uncertainty bounds, zero sample and input
    // counts, and an exactly-16-character digest: four boundaries at once.
    exec(
        conn,
        "window_calibration_candidate",
        "INSERT INTO window_calibration_candidate (id, candidate_id, experiment_id, provider, plan_tier, window_semantic_key, fitted_micros_per_point, equivalent_full_window_capacity_micros, fit_residual_micros, uncertainty_low_micros, uncertainty_high_micros, sample_count, inputs_digest, inputs_count, valid_from, valid_until, knowledge_time) VALUES
            (1, 'matrix-candidate-1', 1, 'anthropic', 'pro', 'matrix-key-1', 500, 1000000, 0, 3, 3, 0, '0123456789abcdef', 0, 0, 0, 10)",
    )
}

fn populate_window_calibration_result(conn: &rusqlite::Connection) -> Result<(), String> {
    // Every nullable column left null: the all-absent edge.
    exec(
        conn,
        "window_calibration_result",
        "INSERT INTO window_calibration_result (id, calibration_id, provider, plan_tier, window_semantic_key, meter_semantics_id, billing_semantics_id, cost_model_id, fitted_micros_per_point, equivalent_full_window_capacity_micros, fit_residual_micros, uncertainty_low_micros, uncertainty_high_micros, lag_estimate_nanos, lag_handling, sample_count, fit_timestamp, inputs_digest, inputs_count, fitting_evidence_digest, validation_evidence_digest, validation_method, validation_version, out_of_sample_residual_micros, statistical_method, statistical_parameters, condition_number_micros, observation_coverage_requirement, settling_policy, excluded_samples, activation_policy_version, aub_version, source_revision, valid_from, valid_until, knowledge_time) VALUES
            (1, 'matrix-calibration-1', 'anthropic', 'pro', 'matrix-key-1', 'matrix-semantics', 'matrix-billing', 'matrix-cm-1', 500, 1000000, 0, 3, 3, NULL, 'none', 5, 20, '0123456789abcdef', 2, 'aaaaaaaaaaaaaaaa', 'bbbbbbbbbbbbbbbb', 'holdout', 'matrix-validation-v1', NULL, 'ols', '{}', NULL, 'matrix-coverage', 'matrix-settling', '[]', 'matrix-activation-v1', '0.0.0-matrix', 'matrix-revision', 0, 0, 10)",
    )
}

fn populate_window_calibration_source_experiment(
    conn: &rusqlite::Connection,
) -> Result<(), String> {
    exec(
        conn,
        "window_calibration_source_experiment",
        "INSERT INTO window_calibration_source_experiment (id, result_id, experiment_id) VALUES
            (1, 1, 1)",
    )
}

fn populate_calibration_lifecycle(conn: &rusqlite::Connection) -> Result<(), String> {
    exec(
        conn,
        "calibration_lifecycle",
        "INSERT INTO calibration_lifecycle (id, calibration_result_id, event_kind, event_at, supersedes_result_id) VALUES
            (1, 1, 'activation', 30, NULL)",
    )
}

fn populate_attribution_segment(conn: &rusqlite::Connection) -> Result<(), String> {
    // Row 1 is a task target with every token count at the zero floor; row 2
    // is the overhead target with its reason set and both task columns
    // absent, the other arm of the pairing CHECK.
    exec(
        conn,
        "attribution_segment",
        "INSERT INTO attribution_segment (id, session_id, target_kind, task_source, task_native, overhead_reason, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, computed_at) VALUES
            (1, 'matrix-native-1', 'task', 'github', 'matrix-issue-7', NULL, 0, 0, 0, 0, 900),
            (2, 'matrix-native-1', 'overhead', NULL, NULL, 'matrix-idle', 5, 6, 7, 8, 900)",
    )
}

fn populate_ingest_quarantine(conn: &rusqlite::Connection) -> Result<(), String> {
    // Row 1 leaves the byte offset, line number and excerpt absent with the
    // observation window closed (last equals first); row 2 puts zero into
    // both offsets and fills the excerpt.
    exec(
        conn,
        "ingest_quarantine",
        "INSERT INTO ingest_quarantine (id, source_file, byte_offset, line_number, parser, failure_class, excerpt_hash, excerpt, first_observed, last_observed) VALUES
            (1, 'rel/alpha.jsonl', NULL, NULL, 'matrix-parser', 'malformed', 'matrix-excerpt-hash-1', NULL, 0, 0),
            (2, 'rel/beta.jsonl', 0, 0, 'matrix-parser', 'truncated', 'matrix-excerpt-hash-2', 'partial line', 1, 1)",
    )
}

fn populate_ingestion_generation(conn: &rusqlite::Connection) -> Result<(), String> {
    exec(
        conn,
        "ingestion_generation",
        "UPDATE ingestion_generation SET generation = 9223372036854775807 WHERE id = 1",
    )
}

fn populate_legacy_meter_import(conn: &rusqlite::Connection) -> Result<(), String> {
    exec(
        conn,
        "legacy_meter_import",
        "INSERT INTO legacy_meter_import (source_digest, verified_backup_id, imported_at, records_read, records_quarantined) VALUES
            ('1111111111111111111111111111111111111111111111111111111111111111', 'matrix-backup-1', 1000, 2, 0),
            ('2222222222222222222222222222222222222222222222222222222222222222', 'matrix-backup-2', 2000, 1, 1)",
    )
}

fn populate_legacy_meter_import_record(conn: &rusqlite::Connection) -> Result<(), String> {
    exec(
        conn,
        "legacy_meter_import_record",
        "INSERT INTO legacy_meter_import_record (source_digest, source_line, observation_id, marker_id) VALUES
            ('1111111111111111111111111111111111111111111111111111111111111111', 1, 1, 1),
            ('1111111111111111111111111111111111111111111111111111111111111111', 2, 2, 2)",
    )
}

/// Applies the population entries in registry order (parents first), skipping
/// tables absent from this schema version, then refuses any present table the
/// registry does not cover and any entry that inserted nothing. The refusal
/// is the structural gate: a migration that adds a table without teaching the
/// matrix what a row of it looks like fails the suite here.
fn populate_tables(
    conn: &rusqlite::Connection,
    tables: &[String],
    population: &[(&str, Populate)],
) -> Result<(), String> {
    // Coverage first, before any insert runs: an uncovered table must be the
    // failure the matrix reports, not a duplicate-key error from an entry
    // that already ran.
    for table in tables {
        if !population.iter().any(|(name, _)| name == table) {
            return Err(format!(
                "fixture population: table {table} has no population entry; the migration matrix cannot exercise it: add a populate function for {table} to POPULATION in tests/migration_matrix.rs"
            ));
        }
    }
    for (name, populate) in population {
        if !tables.iter().any(|table| table == name) {
            continue;
        }
        populate(conn)?;
    }
    for table in tables {
        if row_count(conn, table)? == 0 {
            return Err(format!(
                "fixture population: the entry for {table} inserted no rows; the matrix requires rows in every table"
            ));
        }
    }
    Ok(())
}

fn populate_fixture(conn: &rusqlite::Connection) -> Result<(), String> {
    populate_tables(conn, &user_tables(conn), POPULATION)
}

// ---------------------------------------------------------------------------
// The matrix machinery
// ---------------------------------------------------------------------------

/// One matrix row's outcome: the version it started from, the migrations it
/// applied, and how long the row took.
struct MatrixRow {
    start: u32,
    applied: Vec<u32>,
    elapsed: Duration,
}

/// A verified backup of the fixture, archived at the schema version it was
/// captured from. `verified_backup_exists` re-verifies the archive on every
/// ask rather than trusting a remembered answer, and requires the archive's
/// recorded schema version to match the version asked about, because a backup
/// from a different schema generation never satisfies the rewrite guard.
struct VerifiedFixtureBackup {
    archive: PathBuf,
}

impl VerifiedBackup for VerifiedFixtureBackup {
    fn verified_backup_exists(&self, schema_version: u32) -> bool {
        match backup::verify_database(&self.archive, policy().busy_timeout) {
            Ok(Ok(_)) => {}
            _ => return false,
        }
        match backup::archived_database_metadata(&self.archive, policy().busy_timeout) {
            Ok((recorded, _)) => recorded == schema_version,
            Err(_) => false,
        }
    }
}

/// Captures a consistent SQLite snapshot of `source` and verifies it before
/// returning, so a successful capture is by construction a verified backup
/// (PLAN.md section 38: an unverified archive is not yet a backup).
fn capture_verified_fixture_backup(
    source: &rusqlite::Connection,
    destination: &Path,
) -> Result<VerifiedFixtureBackup, String> {
    let mut dest = open(destination, AccessMode::ReadWrite, &policy())
        .map_err(|e| format!("cannot open the backup destination {destination:?}: {e}"))?;
    let handle = rusqlite::backup::Backup::new(source, &mut dest)
        .map_err(|e| format!("cannot start the SQLite backup: {e}"))?;
    handle
        .run_to_completion(32, Duration::from_millis(1), None)
        .map_err(|e| format!("cannot complete the SQLite backup: {e}"))?;
    drop(handle);
    dest.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .map_err(|e| format!("cannot checkpoint the backup database: {e}"))?;
    drop(dest);
    match backup::verify_database(destination, policy().busy_timeout)
        .map_err(|e| format!("cannot verify the fixture backup: {e}"))?
    {
        Ok(_) => Ok(VerifiedFixtureBackup {
            archive: destination.to_path_buf(),
        }),
        Err(failure) => Err(format!(
            "the fixture backup failed verification at stage {}: {}",
            failure.stage.as_str(),
            failure.detail
        )),
    }
}

/// Applies exactly the migration with version `target`, capturing and
/// verifying a backup first when that migration rewrites an irreplaceable
/// table. This is the operational sequence of PLAN.md section 11.4: back up,
/// verify, migrate.
fn migrate_forward_one_step(
    conn: &mut rusqlite::Connection,
    migrations: &[Migration],
    target: u32,
    backup_destination: &Path,
) -> Result<MigrationSummary, String> {
    let next = &migrations[(target - 1) as usize];
    assert_eq!(next.version, target, "the registry must be consecutive");
    let backup_verifier = if next.rewrites_irreplaceable {
        Some(capture_verified_fixture_backup(conn, backup_destination)?)
    } else {
        None
    };
    run_migrations(
        conn,
        &migrations[..target as usize],
        backup_verifier.as_ref().map(|b| b as &dyn VerifiedBackup),
        &clock(),
    )
    .map_err(|e| format!("migration to version {target} failed: {e}"))
}

/// The two post-migration checks the matrix runs after every applied
/// migration, with failures named so a red run says which check and which
/// version caught the database.
fn check_post_migration(conn: &rusqlite::Connection, version: u32) -> Result<(), String> {
    let messages = match collect_integrity_messages(conn) {
        Ok(messages) => messages,
        Err(e) => {
            return Err(format!(
                "integrity_check after migration to version {version} could not run: {e}"
            ));
        }
    };
    if messages.as_slice() != ["ok"] {
        return Err(format!(
            "integrity_check after migration to version {version} failed: {messages:?}"
        ));
    }
    let violations = collect_foreign_key_violations(conn).map_err(|e| {
        format!("foreign_key_check after migration to version {version} could not run: {e}")
    })?;
    if !violations.is_empty() {
        return Err(format!(
            "foreign_key_check after migration to version {version} failed: {violations:?}"
        ));
    }
    Ok(())
}

fn collect_integrity_messages(conn: &rusqlite::Connection) -> Result<Vec<String>, String> {
    let mut statement = conn
        .prepare("PRAGMA integrity_check")
        .map_err(|e| format!("cannot prepare integrity_check: {e}"))?;
    statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|e| format!("integrity_check errored: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("integrity_check errored: {e}"))
}

fn collect_foreign_key_violations(conn: &rusqlite::Connection) -> Result<Vec<String>, String> {
    let mut statement = conn
        .prepare("PRAGMA foreign_key_check")
        .map_err(|e| format!("cannot prepare foreign_key_check: {e}"))?;
    statement
        .query_map([], |row| {
            Ok(format!(
                "table={} rowid={} parent={} fkid={}",
                row.get::<_, String>(0)?,
                row.get::<_, Option<i64>>(1)?
                    .map_or_else(|| "none".to_owned(), |value| value.to_string()),
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })
        .map_err(|e| format!("foreign_key_check errored: {e}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("foreign_key_check errored: {e}"))
}

/// An additive migration must not touch a single row. A rewrite may (that is
/// what a rewrite is for), so its steps skip this assertion and are judged by
/// the rewrite scenarios instead.
fn assert_counts_unchanged(
    conn: &rusqlite::Connection,
    baseline: &[(String, i64)],
    version: u32,
) -> Result<(), String> {
    for (table, before) in baseline {
        let after = row_count(conn, table)?;
        if after != *before {
            return Err(format!(
                "migration to version {version} changed the row count of {table} from {before} to {after}; an additive migration must not touch rows"
            ));
        }
    }
    Ok(())
}

/// Opens the fixture database, migrates it to `start`, fills every table
/// present with fixture rows, and snapshots the baseline row counts.
fn open_populated_fixture(
    db: &FixtureDb,
    migrations: &[Migration],
    start: u32,
) -> Result<(rusqlite::Connection, Vec<(String, i64)>), String> {
    let mut conn = open(&db.path, AccessMode::ReadWrite, &policy())
        .map_err(|e| format!("matrix row {start}: cannot open the fixture database: {e}"))?;
    assert_foreign_keys_enforced(&conn)?;
    for target in 1..=start {
        migrate_forward_one_step(&mut conn, migrations, target, &db.backup_path())?;
    }
    populate_fixture(&conn)?;
    let baseline = snapshot_row_counts(&conn, &user_tables(&conn))?;
    Ok((conn, baseline))
}

/// The upgrade half of one matrix row: from the populated fixture at `start`,
/// apply migrations one at a time to the registry's highest version, checking
/// the recorded version, integrity, foreign keys and row counts after every
/// step. The checks live here, inside the row flow, so the negative tests
/// below prove they run where they claim to.
fn upgrade_from(
    conn: &mut rusqlite::Connection,
    db: &FixtureDb,
    migrations: &[Migration],
    start: u32,
    baseline: &[(String, i64)],
) -> Result<Vec<u32>, String> {
    let highest = migrations
        .last()
        .map(|migration| migration.version)
        .unwrap_or(0);
    let mut applied = Vec::new();
    for target in (start + 1)..=highest {
        let next = &migrations[(target - 1) as usize];
        let summary = migrate_forward_one_step(conn, migrations, target, &db.backup_path())?;
        if summary.applied != vec![target] {
            return Err(format!(
                "matrix row {start}: migration to version {target} applied {:?} instead of exactly [{target}]",
                summary.applied
            ));
        }
        check_post_migration(conn, target)?;
        if !next.rewrites_irreplaceable {
            assert_counts_unchanged(conn, baseline, target)?;
        }
        applied.push(target);
    }
    if applied.is_empty() {
        // A database already at the highest version still gets its checks: the
        // fixture is populated and must be healthy as it stands.
        check_post_migration(conn, start)?;
    }
    Ok(applied)
}

/// One matrix row: build the populated fixture at `start`, then upgrade it to
/// the registry's highest version.
fn run_matrix_row(
    db: &FixtureDb,
    migrations: &[Migration],
    start: u32,
) -> Result<Vec<u32>, String> {
    let (mut conn, baseline) = open_populated_fixture(db, migrations, start)?;
    upgrade_from(&mut conn, db, migrations, start, &baseline)
}

/// The full matrix: one row per schema version from 0 through the registry's
/// highest. Migration `m` is applied in exactly the rows whose start precedes
/// it, so every migration is exercised from every prior schema version.
fn run_matrix(migrations: &[Migration]) -> Result<Vec<MatrixRow>, String> {
    let highest = migrations
        .last()
        .map(|migration| migration.version)
        .unwrap_or(0);
    let mut rows = Vec::new();
    for start in 0..=highest {
        let started = Instant::now();
        let db = FixtureDb::new("matrix");
        let applied = run_matrix_row(&db, migrations, start)?;
        rows.push(MatrixRow {
            start,
            applied,
            elapsed: started.elapsed(),
        });
    }
    Ok(rows)
}

/// Builds a populated fixture database at `version` and drops its connection,
/// so the caller can inspect or damage the file before reopening it. Returns
/// the baseline row counts.
fn build_populated_fixture(
    db: &FixtureDb,
    migrations: &[Migration],
    version: u32,
) -> Result<Vec<(String, i64)>, String> {
    let (conn, counts) = open_populated_fixture(db, migrations, version)?;
    drop(conn);
    Ok(counts)
}

/// The production registry with one synthetic migration appended: a rewrite
/// carrying the rows of `sampling_lease` into a fresh table. No registry
/// migration is a rewrite today, so this is the populated rewrite the
/// with-and-without-backup scenarios need, over real fixture data.
fn synthetic_rewrite_apply(conn: &rusqlite::Connection) -> Result<(), Error> {
    conn.execute_batch(
        "CREATE TABLE sampling_lease_rewrite (
            account_name TEXT PRIMARY KEY,
            holder TEXT NOT NULL,
            acquired_at INTEGER NOT NULL,
            expires_at INTEGER NOT NULL,
            CHECK (length(account_name) > 0),
            CHECK (length(holder) > 0),
            CHECK (expires_at > acquired_at)
        ) STRICT;
        INSERT INTO sampling_lease_rewrite (account_name, holder, acquired_at, expires_at)
            SELECT account_name, holder, acquired_at, expires_at FROM sampling_lease;
        DROP TABLE sampling_lease;
        ALTER TABLE sampling_lease_rewrite RENAME TO sampling_lease;",
    )
    .map_err(|e| Error::Store(format!("synthetic rewrite failed: {e}")))
}

fn registry_with_a_synthetic_rewrite() -> Vec<Migration> {
    let mut migrations = registry();
    let next_version = migrations.last().map(|m| m.version).unwrap_or(0) + 1;
    migrations.push(Migration {
        version: next_version,
        rewrites_irreplaceable: true,
        apply: synthetic_rewrite_apply,
    });
    migrations
}

// ---------------------------------------------------------------------------
// The matrix itself
// ---------------------------------------------------------------------------

/// Integration: the matrix applies every migration forward from every prior
/// schema version against a populated fixture database, with post-migration
/// integrity and foreign-key checking after every step. Performance: the full
/// matrix completes within its documented CI time budget.
#[test]
fn the_matrix_applies_every_migration_forward_from_every_prior_schema_version() {
    let migrations = registry();
    let started = Instant::now();
    let rows = run_matrix(&migrations).expect("the migration matrix must pass");
    let elapsed = started.elapsed();

    let highest = migrations.last().map(|m| m.version).unwrap_or(0);
    assert_eq!(
        rows.len(),
        highest as usize + 1,
        "one matrix row per schema version, 0 through {highest}"
    );
    // Migration m is applied from every prior version: starts 0..=m-1, which
    // is exactly m rows. A skipped row or a version unreachable from some
    // prior version breaks this count.
    for migration in &migrations {
        let from = rows
            .iter()
            .filter(|row| row.applied.contains(&migration.version))
            .count();
        assert_eq!(
            from, migration.version as usize,
            "migration {} must be applied from every one of its {} prior versions",
            migration.version, migration.version
        );
    }
    let applications: usize = rows.iter().map(|row| row.applied.len()).sum();
    for row in &rows {
        println!(
            "matrix row from version {:2}: {:2} applications in {:?}",
            row.start,
            row.applied.len(),
            row.elapsed
        );
    }
    println!(
        "migration matrix: {} rows, {applications} migration applications, total {elapsed:?}, budget {:?}",
        rows.len(),
        MATRIX_CI_TIME_BUDGET
    );
    assert!(
        elapsed <= MATRIX_CI_TIME_BUDGET,
        "the migration matrix took {elapsed:?}, over the {}s budget",
        MATRIX_CI_TIME_BUDGET.as_secs()
    );
}

/// Unit: the structural gate. Every migration file under src/store/migrations
/// has a registry entry, and every registry entry has its file. A migration
/// file added without registering it is invisible to `registry()` and so to
/// the matrix; this check makes that omission a red suite rather than a
/// silent skip.
#[test]
fn every_migration_file_has_a_registry_entry_and_every_registry_entry_has_a_file() {
    let migrations_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/store/migrations");
    let mut file_versions: Vec<(u32, String)> = Vec::new();
    for entry in std::fs::read_dir(&migrations_dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", migrations_dir.display()))
    {
        let path = entry.expect("directory entries must read").path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(stem) = name.strip_suffix(".rs") else {
            continue;
        };
        // A migration file is NNNN_something.rs; anything else (mod.rs) is
        // not a migration step.
        let digits = &stem[..stem.len().min(4)];
        if digits.len() != 4
            || !digits.bytes().all(|b| b.is_ascii_digit())
            || !stem[4..].starts_with('_')
        {
            continue;
        }
        let version: u32 = digits
            .parse()
            .unwrap_or_else(|e| panic!("migration file {name}: bad version number: {e}"));
        file_versions.push((version, name.to_owned()));
    }
    assert!(
        !file_versions.is_empty(),
        "the migrations directory holds no migration files"
    );

    let migrations = registry();
    for (version, file) in &file_versions {
        assert!(
            migrations.iter().any(|m| m.version == *version),
            "migration file {file} declares version {version} but the registry in src/store/migrations/mod.rs has no entry for it; the matrix cannot exercise what the registry does not declare"
        );
    }
    for migration in &migrations {
        assert!(
            file_versions.iter().any(|(v, _)| v == &migration.version),
            "registry entry for version {} has no migration file in src/store/migrations",
            migration.version
        );
    }
    file_versions.sort();
    for window in file_versions.windows(2) {
        assert!(
            window[0].0 != window[1].0,
            "version {} is declared by two files: {} and {}",
            window[0].0,
            window[0].1,
            window[1].1
        );
    }
}

/// Unit: the population gate. A table the matrix has no entry for fails the
/// population step, naming the table, so a migration that adds a table cannot
/// land without the matrix learning its shape.
#[test]
fn a_table_without_a_population_entry_fails_the_matrix() {
    let migrations = registry();
    let highest = migrations.last().map(|m| m.version).unwrap_or(0);
    let db = FixtureDb::new("unpopulated");
    build_populated_fixture(&db, &migrations, highest).expect("the fixture must build");

    let conn = open(&db.path, AccessMode::ReadWrite, &policy()).unwrap();
    conn.execute_batch("CREATE TABLE matrix_unpopulated_probe (id INTEGER PRIMARY KEY) STRICT")
        .unwrap();
    let error = populate_fixture(&conn).expect_err("an unpopulated table must fail the matrix");
    assert!(
        error.contains("matrix_unpopulated_probe"),
        "the failure must name the uncovered table: {error}"
    );
    assert!(
        error.contains("no population entry"),
        "the failure must say what is missing: {error}"
    );
}

/// Unit: the row-count half of the population gate. An entry that exists but
/// inserts nothing fails, because "rows in every table" is asserted, not
/// assumed.
#[test]
fn a_population_entry_that_inserts_no_rows_fails_the_matrix() {
    fn populate_nothing(_conn: &rusqlite::Connection) -> Result<(), String> {
        Ok(())
    }
    let population = [("matrix_zero_rows_probe", populate_nothing as Populate)];

    let db = FixtureDb::new("zero-rows");
    let conn = open(&db.path, AccessMode::ReadWrite, &policy()).unwrap();
    conn.execute_batch("CREATE TABLE matrix_zero_rows_probe (id INTEGER PRIMARY KEY) STRICT")
        .unwrap();
    let error = populate_tables(&conn, &["matrix_zero_rows_probe".to_owned()], &population)
        .expect_err("an entry that inserts no rows must fail");
    assert!(
        error.contains("matrix_zero_rows_probe") && error.contains("inserted no rows"),
        "the failure must name the empty table: {error}"
    );
}

/// Integration: the synthetic rewrite proceeds over a populated fixture when
/// a verified backup exists at the version in force, every row it rewrote
/// arrives intact, and no other table loses a row.
#[test]
fn a_rewrite_migration_proceeds_with_a_verified_backup_and_the_rows_survive() {
    let migrations = registry_with_a_synthetic_rewrite();
    let rewrite_version = migrations.last().unwrap().version;
    let db = FixtureDb::new("rewrite-proceeds");
    let baseline = build_populated_fixture(&db, &migrations, rewrite_version - 1)
        .expect("the fixture must build");

    let mut conn = open(&db.path, AccessMode::ReadWrite, &policy()).unwrap();
    let verified = capture_verified_fixture_backup(&conn, &db.backup_path())
        .expect("the fixture backup must capture and verify");
    let summary = run_migrations(&mut conn, &migrations, Some(&verified), &clock())
        .expect("the rewrite must proceed with a verified backup present");
    assert_eq!(summary.applied, vec![rewrite_version]);
    check_post_migration(&conn, rewrite_version).expect("the rewritten database must be healthy");

    // The rewrite carried the fixture's rows across verbatim.
    let leases: Vec<(String, String, i64, i64)> = conn
        .prepare(
            "SELECT account_name, holder, acquired_at, expires_at \
             FROM sampling_lease ORDER BY account_name",
        )
        .unwrap()
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(
        leases,
        vec![("alpha".to_owned(), "matrix-holder".to_owned(), 0, 1)],
        "the rewrite must carry the fixture's rows across verbatim"
    );
    for (table, before) in &baseline {
        assert_eq!(
            row_count(&conn, table).unwrap(),
            *before,
            "the rewrite touched rows in {table}, which it does not own"
        );
    }
}

/// Integration: the same rewrite refuses to start without a verified backup,
/// leaving the fixture database at the prior version with every row intact.
#[test]
fn a_rewrite_migration_refuses_without_a_verified_backup_over_a_populated_fixture() {
    let migrations = registry_with_a_synthetic_rewrite();
    let rewrite_version = migrations.last().unwrap().version;
    let db = FixtureDb::new("rewrite-refusal");
    let baseline = build_populated_fixture(&db, &migrations, rewrite_version - 1)
        .expect("the fixture must build");

    let mut conn = open(&db.path, AccessMode::ReadWrite, &policy()).unwrap();
    let error = run_migrations(&mut conn, &migrations, None, &clock())
        .expect_err("no verifier wired at all must refuse the rewrite");
    let message = error.to_string();
    assert!(
        message.contains("rewrites irreplaceable") && message.contains("no verified backup"),
        "the refusal must name the precondition: {message}"
    );
    assert_eq!(
        current_schema_version(&conn).unwrap(),
        rewrite_version - 1,
        "the refused migration must leave the version where it was"
    );
    check_post_migration(&conn, rewrite_version - 1)
        .expect("the refused database must still be healthy");
    for (table, before) in &baseline {
        assert_eq!(
            row_count(&conn, table).unwrap(),
            *before,
            "the refusal must leave {table} untouched"
        );
    }
}

/// Integration: a verified backup taken two versions earlier does not satisfy
/// the rewrite guard, because the guard asks about the version in force and a
/// backup from another schema generation is not a backup of this one.
#[test]
fn a_rewrite_migration_refuses_when_the_verified_backup_predates_the_schema_version() {
    let migrations = registry_with_a_synthetic_rewrite();
    let rewrite_version = migrations.last().unwrap().version;
    let db = FixtureDb::new("rewrite-stale-backup");
    let baseline = build_populated_fixture(&db, &migrations, rewrite_version - 2)
        .expect("the fixture must build");

    let mut conn = open(&db.path, AccessMode::ReadWrite, &policy()).unwrap();
    // Verified at rewrite_version - 2, then one ordinary migration advances
    // the database without invalidating the archive file itself.
    let stale = capture_verified_fixture_backup(&conn, &db.backup_path())
        .expect("the backup must capture and verify");
    run_migrations(
        &mut conn,
        &migrations[..(rewrite_version - 1) as usize],
        None,
        &clock(),
    )
    .expect("the ordinary migration must apply");
    let error = run_migrations(&mut conn, &migrations, Some(&stale), &clock())
        .expect_err("a backup from an older schema generation must not satisfy the guard");
    let message = error.to_string();
    assert!(
        message.contains("no verified backup"),
        "the refusal must name the precondition: {message}"
    );
    assert!(
        message.contains(&format!("schema version {}", rewrite_version - 1)),
        "the refusal must name the version it was asked about: {message}"
    );
    check_post_migration(&conn, rewrite_version - 1)
        .expect("the refused database must still be healthy");
    for (table, before) in &baseline {
        assert_eq!(row_count(&conn, table).unwrap(), *before);
    }
}

/// Integration: every registry migration marked as a rewrite is run through
/// the refusal flow over a populated fixture. The registry has no rewrite
/// migrations today, so the loop is empty and the synthetic rewrite tests
/// carry the proof; the loop is what turns a future rewrite's refusal into a
/// matrix case automatically.
#[test]
fn every_rewrite_migration_in_the_registry_refuses_without_a_verified_backup() {
    let migrations = registry();
    let rewrites: Vec<&Migration> = migrations
        .iter()
        .filter(|migration| migration.rewrites_irreplaceable)
        .collect();
    if rewrites.is_empty() {
        println!(
            "no migration in the registry rewrites an irreplaceable table yet; \
             the synthetic rewrite tests carry the refusal proof"
        );
        return;
    }
    for rewrite in rewrites {
        let db = FixtureDb::new("registry-rewrite-refusal");
        let baseline = build_populated_fixture(&db, &migrations, rewrite.version - 1)
            .expect("the fixture must build");
        let mut conn = open(&db.path, AccessMode::ReadWrite, &policy()).unwrap();
        let error = run_migrations(&mut conn, &migrations, None, &clock()).expect_err(&format!(
            "expected the refusal of migration {}",
            rewrite.version
        ));
        assert!(
            error.to_string().contains("no verified backup"),
            "the refusal must name the precondition: {error}"
        );
        for (table, before) in &baseline {
            assert_eq!(row_count(&conn, table).unwrap(), *before);
        }
    }
}

/// Negative: the matrix's foreign-key check is live, inside the row flow. An
/// orphan planted in the fixture before the upgrade is caught by the check
/// the matrix runs after the first applied migration, naming the offending
/// table. A matrix that skipped its checks would pass the main test and fail
/// here.
#[test]
fn the_matrix_refuses_a_fixture_whose_rows_already_violate_a_foreign_key() {
    let migrations = registry();
    let db = FixtureDb::new("planted-orphan");
    let (mut conn, baseline) =
        open_populated_fixture(&db, &migrations, 13).expect("the fixture must build");

    // Plant the orphan with foreign keys relaxed for the one statement. The
    // matrix's own connections always run with enforcement on, so the planted
    // row is invisible to inserts and visible to foreign_key_check.
    conn.pragma_update(None, "foreign_keys", "OFF").unwrap();
    conn.execute(
        "UPDATE session_account_marker SET resolved_account_id = 424242 WHERE id = 1",
        [],
    )
    .unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();

    let error = upgrade_from(&mut conn, &db, &migrations, 13, &baseline)
        .expect_err("an orphan planted before the upgrade must fail the matrix");
    assert!(
        error.contains("foreign_key_check") && error.contains("session_account_marker"),
        "the matrix must catch the orphan by name: {error}"
    );
}

/// Negative: the matrix's integrity check is live, inside the row flow. A
/// fixture whose file is damaged before the upgrade is caught by the
/// integrity check the matrix runs after the first applied migration, rather
/// than being blessed as a successful upgrade path.
#[test]
fn the_matrix_refuses_a_fixture_with_a_damaged_page() {
    let migrations = registry();
    let db = FixtureDb::new("damaged-page");
    let (_conn, baseline) =
        open_populated_fixture(&db, &migrations, 17).expect("the fixture must build");
    drop(_conn);
    corrupt_an_index_leaf_page(&db.path);

    let mut conn = open(&db.path, AccessMode::ReadWrite, &policy()).unwrap();
    let error = upgrade_from(&mut conn, &db, &migrations, 17, &baseline)
        .expect_err("a damaged fixture must fail the matrix's integrity check");
    assert!(
        error.contains("integrity_check"),
        "the matrix must catch the damage through its integrity check: {error}"
    );
}

/// Damages the first index leaf page in the file. An index page is chosen
/// because ordinary table reads stay functional while integrity_check walks
/// every index, so the failure surfaces through the integrity check itself.
/// The page's type byte (0x0a: leaf index) is replaced with an invalid one.
fn corrupt_an_index_leaf_page(path: &Path) {
    use std::io::Read;
    let mut data = Vec::new();
    std::fs::File::open(path)
        .unwrap_or_else(|e| panic!("cannot open {path:?}: {e}"))
        .read_to_end(&mut data)
        .unwrap();
    let header_page_size = u16::from_be_bytes([data[16], data[17]]) as usize;
    let page_size = if header_page_size == 1 {
        65536
    } else {
        header_page_size
    };
    let pages = data.len() / page_size;
    let target = (1..pages)
        .find(|page| data[page * page_size] == 0x0a)
        .unwrap_or_else(|| panic!("{path:?} holds no index leaf page to damage"));
    data[target * page_size] = 0x7f;
    std::fs::write(path, &data)
        .unwrap_or_else(|e| panic!("cannot write the damaged fixture {path:?}: {e}"));
}
