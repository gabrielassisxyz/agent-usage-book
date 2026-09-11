# aub-n27.10: the finalized doctor registry over a fully seeded degraded state
# directory. Every expected check is asserted in both human and versioned JSON
# output, the exit class stays success (doctor reports, it does not gate), and
# `doctor --fix` repairs only the safe boundary while irreparable conditions
# keep failing.
#
# Division of proof, stated so the next reader does not re-derive it: the
# per-check forced-failure proof lives in tests/doctor.rs (one executable fail
# case per check, including the residual discrepancy mapping and the
# meter-anomaly horizon split). This case proves the wiring instead: one
# degraded directory, the release binary, every registry entry observed, and
# the repair boundary held.
#
# Two checks are asserted in their honest non-failed states here, with the
# reason why beside each assertion:
# - meter-anomalies passes with zero recorded anomalies (its fail case needs a
#   detected anomaly chain and is forced in tests/doctor.rs instead).
# - unexplained-residual and adapter-semantics-comparison-age are not
#   applicable (no eligible reconciliation interval and no recorded comparison;
#   both fail paths are forced in tests/doctor.rs instead).
#
# The run log is the audit record the bead demands: the runner captures the
# case ID, the exact argv per step, the binary digest, the fixture hashes, the
# sanitized configuration (config path plus digest, never its content), the
# lossless stdout/stderr artifacts, the state digests before and after every
# step, and the assertion results. Cleanup is the runner's own pruning plus a
# state directory that exists only for this run.

CASE_ID="aub-n27.10-doctor-registry"
CASE_DESCRIPTION="aub doctor reports every expected check over a degraded state and --fix holds the safe repair boundary."

CONFIG_FILE=""
LEDGER_DB=""
STATE_ROOT_DIR=""

case_preconditions() {
    require_command "$AUB_BIN"
    require_command sqlite3

    CONFIG_FILE="$STATE_DIR/aub.toml"
    STATE_ROOT_DIR="$STATE_DIR/state"
    LEDGER_DB="$STATE_ROOT_DIR/ledger.db"

    cat > "$CONFIG_FILE" <<EOT
[state]
dir = "$STATE_ROOT_DIR"

[[accounts]]
name = "work-e2e"
provider = "anthropic"
[accounts.credential]
kind = "file"
path = "$STATE_DIR/creds/missing-token.json"

[[transcripts]]
name = "ghost"
root = "$STATE_DIR/transcripts/ghost"
pattern = "**/*.jsonl"

[backup]
destination = "$STATE_DIR/missing-archive"

[drill]
result = "$STATE_DIR/missing-drill.jsonl"
EOT
    mkdir -p "$STATE_ROOT_DIR"

    # Initialize the ledger database schema.
    env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG_FILE" "$AUB_BIN" rebuild transcripts >/dev/null 2>&1 || true

    NOW_SECS="$(date +%s)"
    # Two hours ago: past the 3x5m sampling-cadence threshold, inside the 24h
    # clock-skew and error-classification lookbacks.
    OLD_SECS="$((NOW_SECS - 7200))"
    OLD_NANOS="$((OLD_SECS * 1000000000))"
    OLD_PLUS_60_NANOS="$(((OLD_SECS + 60) * 1000000000))"

    # Degradation: undrained pending evidence (repairable).
    mkdir -p "$STATE_ROOT_DIR/pending"
    cat > "$STATE_ROOT_DIR/pending/attempt-99.json" <<'JSON'
{"account_id":1,"run_id":1,"due_at":1000,"due_reason":"ordinary_cadence","started_at":1000,"status_code":200,"headers":[],"body":"{}","raw_evidence":null}
JSON

    # Degradation: projection ahead of the database (repairable).
    cat > "$STATE_ROOT_DIR/projection" <<'JSON'
{"schema_version":1,"ledger_generation":99999}
JSON

    # Degradation: last sample tick was refused (not repairable by --fix).
    cat > "$STATE_ROOT_DIR/sample-tick.json" <<'JSON'
{"schema_version":1,"started_at_nanos":1000,"outcome":"failed","detail":"cannot start sample run: database is locked (waited up to 5000ms)"}
JSON

    # Degradation: durable sampler failure counts (not repairable by --fix).
    cat > "$STATE_ROOT_DIR/sampling-failure-counts.json" <<'JSON'
{"schema_version":1,"failures":[{"category":"due_lookup_failed","reason":"database disk image is malformed","count":2}]}
JSON

    # Degradation: retained diagnostic bodies (cleared only by operator command).
    # Each file is a retained-body record envelope; the byte counts below are
    # the body lengths (9 and 5), which the doctor summary reports.
    mkdir -p "$STATE_ROOT_DIR/retained-bodies/anthropic/messages"
    cat > "$STATE_ROOT_DIR/retained-bodies/anthropic/messages/00000000000000000001.json" <<'JSON'
{"sequence":1,"provider":"anthropic","source":"messages","captured_at_nanos":1000,"body_bytes":[1,2,3,4,5,6,7,8,9]}
JSON
    cat > "$STATE_ROOT_DIR/retained-bodies/anthropic/messages/00000000000000000002.json" <<'JSON'
{"sequence":2,"provider":"anthropic","source":"messages","captured_at_nanos":1000,"body_bytes":[10,11,12,13,14]}
JSON

    # Degradation: quarantined transcript records, one parser failure and one
    # heuristic-key collision.
    sqlite3 "$LEDGER_DB" "INSERT INTO ingest_quarantine (source_file, parser, failure_class, excerpt_hash, first_observed, last_observed) VALUES ('transcripts/claude.jsonl', 'claude-code', 'malformed_json', 'qhash1', 1000, 1000);"
    sqlite3 "$LEDGER_DB" "INSERT INTO ingest_quarantine (source_file, parser, failure_class, excerpt_hash, first_observed, last_observed) VALUES ('transcripts/codex.jsonl', 'codex', 'dedup_collision', 'qhash2', 1000, 1000);"

    # Degradation: a rate card past its review-due date.
    sqlite3 "$LEDGER_DB" "INSERT INTO rate_card (vendor, model, token_class, rate_micros, currency, billing_basis, effective_start, imported_at, review_due) VALUES ('anthropic', 'claude-opus-4', 'input', 10, 'USD', 'per_million_tokens', '2026-01-01', 1000, '2020-01-01');"

    # Degradation: a fitted calibration scope with no active calibration.
    sqlite3 "$LEDGER_DB" "INSERT INTO window_calibration_result (calibration_id, provider, plan_tier, window_semantic_key, meter_semantics_id, billing_semantics_id, cost_model_id, fitted_micros_per_point, equivalent_full_window_capacity_micros, fit_residual_micros, uncertainty_low_micros, uncertainty_high_micros, lag_estimate_nanos, lag_handling, sample_count, fit_timestamp, inputs_digest, inputs_count, fitting_evidence_digest, validation_evidence_digest, validation_method, validation_version, out_of_sample_residual_micros, statistical_method, statistical_parameters, condition_number_micros, observation_coverage_requirement, settling_policy, excluded_samples, activation_policy_version, aub_version, source_revision, valid_from, valid_until, knowledge_time) VALUES ('wcr-e2e-1', 'anthropic', 'max', 'five_hour', 'm1', 'b1', 'c1', 100, 1000, 10, 90, 110, NULL, 'none', 10, 1000, '0123456789abcdef', 1, '0123456789abcdef', '0123456789abcdef', 'v', '1', NULL, 'ols', '{}', NULL, 'cov', 'set', '[]', 'ap1', '0.1.0', 'rev1', 0, 10000, 1000);"

    # Degradation: unknown-account usage beside attributed usage.
    sqlite3 "$LEDGER_DB" "INSERT INTO account_attribution_segment (session_id, target_kind, logical_account, evidence_class, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, computed_at) VALUES ('claude-code:s1', 'account', 'work-e2e', 'explicit_launcher_or_hook', 60, 0, 0, 0, 1000);"
    sqlite3 "$LEDGER_DB" "INSERT INTO account_attribution_segment (session_id, target_kind, logical_account, evidence_class, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, computed_at) VALUES ('claude-code:s1', 'unknown_account', NULL, 'unattributed', 40, 0, 0, 0, 1000);"

    # One account with two old attempts: a clock-skewed success and a failed
    # attempt carrying its provider error classification.
    sqlite3 "$LEDGER_DB" "INSERT INTO account (id, logical_name, provider_key, first_observed_at, last_observed_at) VALUES (1, 'work-e2e', 'anthropic', $OLD_NANOS, $OLD_NANOS);"
    sqlite3 "$LEDGER_DB" "INSERT INTO sample_run (id, trigger, started_at, aub_version, configuration_fingerprint) VALUES (1, 'manual', $OLD_NANOS, '0.1.0', 'cfg');"
    sqlite3 "$LEDGER_DB" "INSERT INTO sampling_policy_snapshot (id, account_id, effective_at, ordinary_cadence_nanos, freshness_horizon_nanos, reset_edge_policy, retry_backoff_policy, command_budget_nanos, policy_algorithm_version) VALUES (1, 1, $OLD_NANOS, 60000000000, 300000000000, 'none', 'none', 1000000000, 'v1');"
    sqlite3 "$LEDGER_DB" "INSERT INTO meter_attempt (id, run_id, account_id, provider, request_started_at, policy_snapshot_id, due_at, due_reason, provider_contract_id, meter_semantics_id) VALUES (1, 1, 1, 'anthropic', $OLD_NANOS, 1, $OLD_NANOS, 'ordinary_cadence', 'contract-1', 'meter-1');"
    sqlite3 "$LEDGER_DB" "INSERT INTO meter_attempt_result (attempt_id, completed_at, elapsed_nanos, outcome, clock_anomaly) VALUES (1, $OLD_NANOS, 1000, 'success', 1);"
    sqlite3 "$LEDGER_DB" "INSERT INTO meter_attempt (id, run_id, account_id, provider, request_started_at, policy_snapshot_id, due_at, due_reason, provider_contract_id, meter_semantics_id) VALUES (2, 1, 1, 'anthropic', $OLD_PLUS_60_NANOS, 1, $OLD_PLUS_60_NANOS, 'ordinary_cadence', 'contract-1', 'meter-1');"
    sqlite3 "$LEDGER_DB" "INSERT INTO meter_attempt_result (attempt_id, completed_at, elapsed_nanos, outcome, failure_class, retry_after_nanos, sanitized_error_classification, retry_index, clock_anomaly) VALUES (2, $OLD_PLUS_60_NANOS, 1000, 'unreachable', 'rate_limited', 60000000000, 'rate_limit_error: Rate limit exceeded.', NULL, 0);"

    # Degradation: the subscription behind the account changed with no newer
    # stored observation, so readings are being refused right now.
    sqlite3 "$LEDGER_DB" "INSERT INTO meter_subscription_change (account_id, kind, previous_identity, current_identity, detecting_attempt_id, detected_at) VALUES (1, 'changed', 'anthropic:max:fp-old', 'anthropic:pro:fp-new', 1, $OLD_NANOS);"
}

case_steps() {
    step "pre-fix doctor text" env \
        "HOME=$STATE_DIR/home" \
        "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "$AUB_BIN" doctor

    step "pre-fix doctor json" env \
        "HOME=$STATE_DIR/home" \
        "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "$AUB_BIN" doctor --format json

    # No absolute state path may reach either rendering: the state directory
    # lives under the operator's home, so printing it prints the home.
    step "no state path in text" sh -c '! grep -qF "$1" "$2"' _ \
        "$STATE_DIR" "$(step_dir 1)/stdout.bin"

    step "no state path in json" sh -c '! grep -qF "$1" "$2"' _ \
        "$STATE_DIR" "$(step_dir 2)/stdout.bin"

    step "doctor fix text" env \
        "HOME=$STATE_DIR/home" \
        "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "$AUB_BIN" doctor --fix

    step "post-fix doctor text" env \
        "HOME=$STATE_DIR/home" \
        "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "$AUB_BIN" doctor

    step "post-fix doctor json" env \
        "HOME=$STATE_DIR/home" \
        "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "$AUB_BIN" doctor --format json
}

case_assertions() {
    # Step 1: doctor reports, it does not gate: exit success with failures.
    assert_exit 0 1
    assert_stdout_contains 1 "Doctor: 24 checks"
    assert_stdout_contains 1 "[PASS] configuration-validity"
    assert_stdout_contains 1 "[PASS] sqlite-and-schema-health"
    assert_stdout_contains 1 "[PASS] strict-and-constraint-integrity"
    assert_stdout_contains 1 "[FAIL] pending-evidence"
    assert_stdout_contains 1 "[FAIL] sampling-cadence"
    assert_stdout_contains 1 "work-e2e"
    assert_stdout_contains 1 "[FAIL] unresolved-authentication"
    assert_stdout_contains 1 "[FAIL] transcript-roots"
    assert_stdout_contains 1 "ghost"
    assert_stdout_contains 1 "[FAIL] parser-failures"
    assert_stdout_contains 1 "[FAIL] unmapped-accounts"
    assert_stdout_contains 1 "input: 40 unattributed of 100 total"
    assert_stdout_contains 1 "[FAIL] missing-active-calibrations"
    assert_stdout_contains 1 "anthropic/max/five_hour"
    assert_stdout_contains 1 "[FAIL] stale-rate-cards"
    assert_stdout_contains 1 "[FAIL] projection-versus-database-generation"
    assert_stdout_contains 1 "[FAIL] backup-age"
    assert_stdout_contains 1 "[PASS] meter-anomalies: 0 window anomalies recorded"
    assert_stdout_contains 1 "[N/A ] unexplained-residual"
    assert_stdout_contains 1 "[FAIL] heuristic-dedup-counts"
    assert_stdout_contains 1 "[FAIL] clock-skew"
    assert_stdout_contains 1 "[PASS] local-filesystem-and-wal-suitability"
    assert_stdout_contains 1 "[FAIL] accumulated-diagnostic-material"
    assert_stdout_contains 1 "retained bodies: 2 (14 bytes)"
    assert_stdout_contains 1 "anthropic/messages: 2 (14 bytes)"
    assert_stdout_contains 1 "[N/A ] adapter-semantics-comparison-age"
    assert_stdout_contains 1 "[FAIL] last-sample-tick"
    assert_stdout_contains 1 "waited up to 5000ms"
    assert_stdout_contains 1 "[FAIL] sampling-failure-counts"
    assert_stdout_contains 1 "database disk image is malformed"
    assert_stdout_contains 1 "[PASS] meter-error-classifications"
    assert_stdout_contains 1 "rate_limit_error"
    assert_stdout_contains 1 "[FAIL] subscription-identity-change"
    assert_stdout_contains 1 "readings refused"
    assert_stdout_contains 1 "[repairable with --fix]"

    # Step 2: the versioned JSON carries every expected check name.
    assert_exit 0 2
    assert_json_field 2 "command" "doctor"
    assert_json_field 2 "schema" "4"
    for name in configuration-validity sqlite-and-schema-health strict-and-constraint-integrity pending-evidence sampling-cadence unresolved-authentication transcript-roots parser-failures unmapped-accounts missing-active-calibrations stale-rate-cards projection-versus-database-generation backup-age meter-anomalies unexplained-residual heuristic-dedup-counts clock-skew local-filesystem-and-wal-suitability accumulated-diagnostic-material adapter-semantics-comparison-age last-sample-tick sampling-failure-counts meter-error-classifications subscription-identity-change; do
        assert_stdout_contains 2 "\"name\":\"$name\""
    done
    assert_stdout_contains 2 '"name":"unmapped-accounts","status":"fail"'
    assert_stdout_contains 2 '"name":"meter-anomalies","status":"pass"'
    assert_stdout_contains 2 '"name":"unexplained-residual","status":"not_applicable"'
    assert_stdout_contains 2 '"name":"pending-evidence","status":"fail"'

    # Steps 3-4: the leak scans pass.
    assert_exit 0 3
    assert_exit 0 4

    # Step 5: --fix performs exactly the permitted repairs.
    assert_exit 0 5
    assert_stdout_contains 5 "Fix: 4 action(s) performed"
    assert_stdout_contains 5 "drain-pending-evidence"
    assert_stdout_contains 5 "rebuild-projection"

    # Step 6: repaired checks pass; nothing --fix must not touch moved.
    # parser-failures and heuristic-dedup-counts pass because --fix rebuilds
    # the transcript materialization group they live in (their declared
    # repair); the quarantine rows it rebuilt are gone, while the retained
    # bodies only an operator command clears keep accumulated failing.
    assert_exit 0 6
    assert_stdout_contains 6 "[PASS] pending-evidence"
    assert_stdout_contains 6 "[PASS] projection-versus-database-generation"
    assert_stdout_contains 6 "[PASS] parser-failures"
    assert_stdout_contains 6 "[PASS] heuristic-dedup-counts"
    assert_stdout_contains 6 "[FAIL] transcript-roots"
    assert_stdout_contains 6 "[FAIL] unmapped-accounts"
    assert_stdout_contains 6 "[FAIL] missing-active-calibrations"
    assert_stdout_contains 6 "[FAIL] stale-rate-cards"
    assert_stdout_contains 6 "[FAIL] backup-age"
    assert_stdout_contains 6 "[FAIL] clock-skew"
    assert_stdout_contains 6 "[FAIL] accumulated-diagnostic-material"
    assert_stdout_contains 6 "retained bodies: 2 (14 bytes)"
    assert_stdout_contains 6 "quarantine rows: 0"
    assert_stdout_contains 6 "[FAIL] last-sample-tick"
    assert_stdout_contains 6 "[FAIL] sampling-failure-counts"
    assert_stdout_contains 6 "[FAIL] subscription-identity-change"

    # Step 7: JSON reflects the repaired state.
    assert_exit 0 7
    assert_stdout_contains 7 '"name":"pending-evidence","status":"pass"'
    assert_stdout_contains 7 '"name":"projection-versus-database-generation","status":"pass"'
    assert_stdout_contains 7 '"name":"unmapped-accounts","status":"fail"'
    assert_stdout_contains 7 '"name":"transcript-roots","status":"fail"'
}
