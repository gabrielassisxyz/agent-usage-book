# aub-cab.6: `calibrate show`, calibrated spend conversion and `can-run`
# name and use one calibration identifier, then all move to its successor
# without source or configuration changes, through the release binary.
#
# No step in this case uses the network or credentials: the endpoint points
# at an unreachable port, no credential file exists anywhere under the state
# directory, and every consumer runs cached. The meter chain (attempt,
# evidence, observation, one account-wide `five_hour` window) is seeded with
# sqlite3, matching this suite's own convention for every table with no
# ingestion path of its own (`025-now-account-switch-boundary.sh` does the
# same for `session_account_marker`); the binary's own drain publishes the
# projection from those rows before the cached read, so no fixture builder
# links into the path under test. Task history (three completed tasks,
# n=3) is seeded the same way `026-can-run.sh` seeds it.
#
# The spend half keeps the clean single-session shape (1,000,000 input
# tokens on account `spend`: 3.00 credits, exactly 10.0000 points at 30
# micros/point, 30.0000 at 10). The can-run history lives on account
# `work-primary`, so one seeded repository drives all three consumers
# without the two workloads sharing a number.

CASE_ID="029-can-run-calibration-single-source"
CASE_DESCRIPTION="calibrate show, spend --window-equivalent and can-run --cached name one calibration identifier, then all move to its append-only successor with no source or configuration change, without network or credentials."

CONFIG=""
LEDGER_DB=""
NOW_NS=""
RESETS_NS=""
CAPSULE_HASH=""

case_preconditions() {
    require_command "$AUB_BIN"
    require_command sqlite3
    require_command python3

    LEDGER_DB="$STATE_DIR/ledger.db"
    CONFIG="$STATE_DIR/aub.toml"

    mkdir -p "$STATE_DIR/home" "$STATE_DIR/transcripts/claude-code" "$STATE_DIR/tracker"

    cat > "$CONFIG" <<CFG_EOF
state.dir = "$STATE_DIR"

[[accounts]]
name = "spend"
provider = "anthropic"
credential = { kind = "file", path = "$STATE_DIR/creds/token.json" }

[[accounts]]
name = "work-primary"
provider = "anthropic"
credential = { kind = "file", path = "$STATE_DIR/creds/token.json" }

[task_distribution]
min_samples = 3

[[transcripts]]
name = "claude-code"
root = "$STATE_DIR/transcripts/claude-code"
pattern = "**/*.jsonl"
format = "claude-code"

[tracker]
kind = "local"
path = "$STATE_DIR/tracker"
CFG_EOF

    # No credential file is created: every step below must succeed without
    # reading one.
    cat > "$STATE_DIR/transcripts/claude-code/sessions.jsonl" <<'JSONL'
{"type":"assistant","timestamp":"2026-08-25T07:00:00.000Z","sessionId":"s-spend","message":{"id":"m-s-spend","model":"claude-3-5-sonnet","usage":{"input_tokens":1000000,"output_tokens":0,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}
{"type":"assistant","timestamp":"2026-08-25T01:00:00.000Z","sessionId":"s1","message":{"id":"m1","usage":{"input_tokens":1000,"output_tokens":500000,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}
{"type":"assistant","timestamp":"2026-08-25T03:00:00.000Z","sessionId":"s2","message":{"id":"m2","usage":{"input_tokens":1000,"output_tokens":800000,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}
{"type":"assistant","timestamp":"2026-08-25T05:00:00.000Z","sessionId":"s3","message":{"id":"m3","usage":{"input_tokens":1000,"output_tokens":1100000,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}
JSONL

    sqlite3 "$STATE_DIR/tracker/beads.db" <<'SQL'
CREATE TABLE events (
    id INTEGER PRIMARY KEY,
    issue_id TEXT NOT NULL,
    event_type TEXT NOT NULL,
    actor TEXT,
    old_value TEXT,
    new_value TEXT,
    created_at TEXT NOT NULL
);
INSERT INTO events (id, issue_id, event_type, actor, old_value, new_value, created_at) VALUES
 (1, 'aub-1', 'status_changed', 'agent-1', 'open', 'in_progress', '2026-08-25T00:30:00Z'),
 (2, 'aub-1', 'status_changed', 'agent-1', 'in_progress', 'closed', '2026-08-25T02:00:00Z'),
 (3, 'aub-2', 'status_changed', 'agent-1', 'open', 'in_progress', '2026-08-25T02:30:00Z'),
 (4, 'aub-2', 'status_changed', 'agent-1', 'in_progress', 'closed', '2026-08-25T04:00:00Z'),
 (5, 'aub-3', 'status_changed', 'agent-1', 'open', 'in_progress', '2026-08-25T04:30:00Z'),
 (6, 'aub-3', 'status_changed', 'agent-1', 'in_progress', 'closed', '2026-08-25T06:00:00Z');
SQL

    NOW_NS="$(date +%s%N)"
    RESETS_NS="$((NOW_NS + 18000000000000))"
    CAPSULE_HASH="$(python3 -c "import hashlib; print(hashlib.sha256(b'{}').hexdigest())")"
}

case_steps() {
    # 1-2. Land the transcript usage and the task claim/release boundaries.
    step "ingest-transcripts" env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG" \
        "$AUB_BIN" ingest transcripts
    step "task-ingest" env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG" \
        "$AUB_BIN" task ingest

    # 3. Task kinds: no CLI resolves a task's kind yet, so these are seeded
    #    directly, matching this suite's convention.
    step "seed-task-identity" sqlite3 "$LEDGER_DB" "
        INSERT INTO task_identity (
            task_source, task_native, state, kind, winner_origin, evidence,
            normalization_version, size_state, size, size_evidence,
            difficulty_state, difficulty, difficulty_evidence
        ) VALUES
         ('beads', 'aub-1', 'resolved', 'task', 'tracker_field:kind', '{}', 1, 'unknown', NULL, '{}', 'unknown', NULL, '{}'),
         ('beads', 'aub-2', 'resolved', 'task', 'tracker_field:kind', '{}', 1, 'unknown', NULL, '{}', 'unknown', NULL, '{}'),
         ('beads', 'aub-3', 'resolved', 'task', 'tracker_field:kind', '{}', 1, 'unknown', NULL, '{}', 'unknown', NULL, '{}');
    "

    # 4. Explicit launcher-or-hook account markers: the spend session for
    #    `spend`, the three task sessions for `work-primary`.
    step "seed-account-markers" sqlite3 "$LEDGER_DB" "
        INSERT INTO session_account_marker
            (session_source, session_native, observed_at, source_ordering_key,
             logical_account, resolved_account_id, marker_source, run_source,
             run_native, evidence_designation)
        VALUES
         ('claude-code', 's-spend', 1787639400000000000, NULL, 'spend', NULL, 'hook', NULL, NULL, 'launcher_or_hook'),
         ('claude-code', 's1', 1787617800000000000, NULL, 'work-primary', NULL, 'hook', NULL, NULL, 'launcher_or_hook'),
         ('claude-code', 's2', 1787617800000000000, NULL, 'work-primary', NULL, 'hook', NULL, NULL, 'launcher_or_hook'),
         ('claude-code', 's3', 1787617800000000000, NULL, 'work-primary', NULL, 'hook', NULL, NULL, 'launcher_or_hook');
    "

    # 5. The meter chain: one successful attempt with one account-wide
    #    `five_hour` window at 620,000 ppm used (38.0 percent remaining).
    #    Fresh (seeded now) so the cached read accepts it.
    step "seed-meter-chain" sqlite3 "$LEDGER_DB" "
        INSERT INTO account (id, logical_name, provider_key, first_observed_at, last_observed_at)
            VALUES (1, 'work-primary', 'anthropic', $NOW_NS, $NOW_NS);
        INSERT INTO sample_run (id, trigger, started_at, ended_at, aub_version, configuration_fingerprint)
            VALUES (1, 'manual', $NOW_NS, NULL, 'test', 'single-source-can-run');
        INSERT INTO sampling_policy_snapshot
            (id, account_id, effective_at, ordinary_cadence_nanos, freshness_horizon_nanos,
             reset_edge_policy, retry_backoff_policy, command_budget_nanos, policy_algorithm_version)
            VALUES (1, 1, $NOW_NS, 3600000000000, 300000000000, 'lead-60s', 'none', 10000000000, 'v1');
        INSERT INTO meter_attempt
            (id, run_id, account_id, provider, request_started_at, credential_context_id,
             policy_snapshot_id, due_at, due_reason, due_basis_attempt_id, due_basis_result_id,
             provider_contract_id, meter_semantics_id)
            VALUES (1, 1, 1, 'anthropic', $NOW_NS, NULL, 1, $NOW_NS, 'forced_or_manual',
                    NULL, NULL, 'contract-v1', 'meter-v1');
        INSERT INTO meter_attempt_result
            (attempt_id, completed_at, elapsed_nanos, outcome, failure_class, retry_after_nanos,
             sanitized_error_classification, retry_index, clock_anomaly)
            VALUES (1, $NOW_NS + 50000000, 50000000, 'success', NULL, NULL, NULL, NULL, 0);
        INSERT INTO meter_response_evidence
            (id, attempt_id, response_classification, received_at, provider_observed_at_original,
             evidence_capsule, capsule_schema_version, sanitizer_version, content_hash, capture_truncated)
            VALUES (1, 1, '200', $NOW_NS, NULL, '{}', 'capsule-v1', 'san-v1', '$CAPSULE_HASH', 0);
        INSERT INTO meter_observation
            (id, attempt_id, evidence_id, account_id, provider, provider_observed_at, received_at,
             measurement_basis, observed_plan, observed_tier, adapter_version,
             provider_contract_id, meter_semantics_id, normalized_fingerprint)
            VALUES (1, 1, 1, 1, 'anthropic', NULL, $NOW_NS, 'locally_received', NULL, NULL,
                    'adapter-v1', 'contract-v1', 'meter-v1', 'fp-single-source-can-run');
        INSERT INTO meter_observation_preference (evidence_id, meter_semantics_id, current_observation_id)
            VALUES (1, 'meter-v1', 1);
        INSERT INTO meter_window
            (observation_id, semantic_key, scope_kind, scoped_model, quota_used_ppm,
             reported_resolution_ppm, quantization, resets_at, nominal_duration_nanos)
            VALUES (1, 'five_hour', 'account_wide', NULL, 620000, 10000, 'exact',
                    $RESETS_NS, 18000000000000);
    "

    # 6-7. An active, complete cost model and the first calibration: a
    #    conspicuous 30 micros/point, so the arithmetic stays comparable
    #    with the two-consumer proof.
    step "seed-cost-model" env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG" \
        "$AUB_BIN" __cost-model-fixture complete
    step "seed-calibration" env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG" \
        "$AUB_BIN" __calibration-fixture five_hour 30

    # 8-11. Before half: all three consumers name the seeded identifier.
    #    The endpoint is unreachable on purpose: any live fetch fails the
    #    step, proving the cached path reads no network. `-v` surfaces the
    #    structured run diagnostics on stderr.
    step "calibrate-show-before" env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG" \
        "$AUB_BIN" calibrate show
    step "spend-window-before" env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG" \
        "$AUB_BIN" spend --since 2026-08-25 --days 1 --group-by account \
            --window-equivalent five_hour --refresh never
    step "can-run-before" env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG" \
        "AUB_ANTHROPIC_ENDPOINT=http://127.0.0.1:9" \
        "$AUB_BIN" -v can-run --task-kind task --account work-primary --task-model sonnet --cached
    step "can-run-json-before" env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG" \
        "AUB_ANTHROPIC_ENDPOINT=http://127.0.0.1:9" \
        "$AUB_BIN" -v can-run --task-kind task --account work-primary --task-model sonnet \
            --cached --format json

    # 12. Append-only supersession, no source or configuration edit: a third
    #    of the coefficient, so the same credits convert to three times the
    #    points and the headroom shrinks to a third.
    step "seed-calibration-successor" env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG" \
        "$AUB_BIN" __calibration-fixture five_hour 10

    # 13-16. After half: all three consumers name the successor.
    step "calibrate-show-after" env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG" \
        "$AUB_BIN" calibrate show
    step "spend-window-after" env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG" \
        "$AUB_BIN" spend --since 2026-08-25 --days 1 --group-by account \
            --window-equivalent five_hour --refresh never
    step "can-run-after" env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG" \
        "AUB_ANTHROPIC_ENDPOINT=http://127.0.0.1:9" \
        "$AUB_BIN" -v can-run --task-kind task --account work-primary --task-model sonnet --cached
    step "can-run-json-after" env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG" \
        "AUB_ANTHROPIC_ENDPOINT=http://127.0.0.1:9" \
        "$AUB_BIN" -v can-run --task-kind task --account work-primary --task-model sonnet \
            --cached --format json
}

case_assertions() {
    assert_exit 0 1
    assert_exit 0 2
    assert_exit 0 3
    assert_exit 0 4
    assert_exit 0 5
    assert_exit 0 6
    assert_exit 0 7

    assert_exit 0 8
    assert_stdout_contains 8 "active window calibration five_hour-fixture-calibration"
    assert_stdout_contains 8 "fitted:          30 micros/point"

    assert_exit 0 9
    assert_stdout_contains 9 "3.00 credits"
    assert_stdout_contains 9 "[10.0000, 10.0000] percentage points"
    assert_stdout_contains 9 "calibration five_hour-fixture-calibration)"

    assert_exit 0 10
    assert_stdout_contains 10 "calibration #five_hour-fixture-calibration  headroom"
    assert_stdout_contains 10 "#five_hour-fixture-calibration, current"
    assert_stdout_contains 10 "limiting window: five_hour"
    assert_stdout_contains 10 "headroom 11"
    assert_stderr_contains 10 "run_started"
    assert_stderr_contains 10 "report_rendered"

    assert_exit 0 11
    assert_json_field 11 command can-run
    assert_json_field 11 outcome.status ready
    assert_json_field 11 limiting_window five_hour
    assert_json_field 11 outcome.windows[0].calibration_id five_hour-fixture-calibration
    assert_json_field 11 outcome.windows[0].headroom.lower 11400000
    assert_json_field 11 outcome.windows[0].headroom.upper 11400000

    assert_exit 0 12

    assert_exit 0 13
    assert_stdout_contains 13 "active window calibration five_hour-fixture-calibration-1"
    assert_stdout_contains 13 "fitted:          10 micros/point"
    if grep -qE -- "^active window calibration five_hour-fixture-calibration\$" \
        "$(step_dir 13)/stdout.bin"; then
        record_assertion "superseded calibration absent from calibrate show" "absent" "present" "fail"
        CASE_FAILED=1
    else
        record_assertion "superseded calibration absent from calibrate show" "absent" "absent" "pass"
    fi

    assert_exit 0 14
    assert_stdout_contains 14 "[30.0000, 30.0000] percentage points"
    assert_stdout_contains 14 "calibration five_hour-fixture-calibration-1)"

    assert_exit 0 15
    assert_stdout_contains 15 "calibration #five_hour-fixture-calibration-1  headroom"
    assert_stdout_contains 15 "#five_hour-fixture-calibration-1, current"
    assert_stdout_contains 15 "headroom 3"
    assert_stderr_contains 15 "report_rendered"
    if grep -qF -- "#five_hour-fixture-calibration, current" \
        "$(step_dir 15)/stdout.bin"; then
        record_assertion "superseded identifier absent from can-run" "absent" "present" "fail"
        CASE_FAILED=1
    else
        record_assertion "superseded identifier absent from can-run" "absent" "absent" "pass"
    fi

    assert_exit 0 16
    assert_json_field 16 outcome.status ready
    assert_json_field 16 outcome.windows[0].calibration_id five_hour-fixture-calibration-1
    assert_json_field 16 outcome.windows[0].headroom.lower 3800000
    assert_json_field 16 outcome.windows[0].headroom.upper 3800000
}
