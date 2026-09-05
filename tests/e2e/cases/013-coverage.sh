# `aub coverage` is a local-ledger command. It reports attempt and measurement
# coverage against local history, distinguishes scheduler death from credential
# failure in the output, and exits with the threshold class when a configured floor
# is breached.

CASE_ID="013-coverage"
CASE_DESCRIPTION="aub coverage reads only local ledger evidence, distinguishes dead scheduler from credential failure, and enforces threshold exits."

LEDGER_DB=""

aub_coverage() {
    env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "AUB_LOG_LEVEL=off" \
        "$AUB_BIN" coverage "$@"
}

case_preconditions() {
    require_command "$AUB_BIN"
    require_command sqlite3

    LEDGER_DB="$STATE_DIR/ledger.db"

    cat > "$STATE_DIR/aub.toml" <<'EOF'
[coverage]
attempt_floor = 0.98
measurement_floor = 0.95
EOF
}

case_steps() {
    step "coverage-without-ledger" aub_coverage

    # Initialize and migrate the ledger database through the release binary
    step "initialize ledger" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "$AUB_BIN" export --key run-id

    # Seed the ledger with two accounts over the last 24 hours:
    # 1. dead-scheduler: one attempt 23h ago, then silence (long blind gap, low attempt coverage)
    # 2. cred-failing: attempts every hour, but all 24 attempts fail with auth_required
    #    (high attempt coverage, low measurement coverage, detail names auth failure)
    step "seed coverage evidence" bash -c '
        set -eu
        NOW=$(date +%s)
        T0=$((NOW - 86400))
        T0_NS=$((T0 * 1000000000))
        NOW_NS=$((NOW * 1000000000))

        sqlite3 "$1" "
            INSERT INTO sample_run (id, trigger, started_at, ended_at, aub_version, configuration_fingerprint)
            VALUES (1, '\''timer'\'', $T0_NS, $NOW_NS, '\''v0.1.0'\'', '\''cfg-1'\'');

            INSERT INTO account (id, logical_name, provider_key, first_observed_at, last_observed_at)
            VALUES (1, '\''dead-scheduler'\'', '\''provider-a'\'', $T0_NS, $NOW_NS),
                   (2, '\''cred-failing'\'', '\''provider-a'\'', $T0_NS, $NOW_NS);

            INSERT INTO sampling_policy_snapshot (id, account_id, effective_at, ordinary_cadence_nanos, freshness_horizon_nanos, reset_edge_policy, retry_backoff_policy, command_budget_nanos, policy_algorithm_version)
            VALUES (1, 1, $T0_NS - 3600000000000, 3600000000000, 900000000000, '\'''\'', '\'''\'', 30000000000, '\''v1'\''),
                   (2, 2, $T0_NS - 3600000000000, 3600000000000, 900000000000, '\'''\'', '\'''\'', 30000000000, '\''v1'\'');

            -- dead-scheduler: 1 attempt at T0 + 1800s
            INSERT INTO meter_attempt (id, run_id, account_id, provider, request_started_at, policy_snapshot_id, due_at, due_reason, provider_contract_id, meter_semantics_id)
            VALUES (1, 1, 1, '\''provider-a'\'', $T0_NS + 1800000000000, 1, $T0_NS + 1800000000000, '\''ordinary_cadence'\'', '\''schema-v1'\'', '\''sem-v1'\'');
            INSERT INTO meter_attempt_result (attempt_id, completed_at, elapsed_nanos, outcome, clock_anomaly)
            VALUES (1, $T0_NS + 1830000000000, 30000000000, '\''success'\'', 0);
        "

        # cred-failing: 24 attempts strictly inside the window, all auth_required
        for slot in $(seq 0 23); do
            ATTEMPT_ID=$((100 + slot))
            START_NS=$(( (T0 + 300 + slot * 3600) * 1000000000 ))
            FINISH_NS=$(( START_NS + 30000000000 ))
            sqlite3 "$1" "
                INSERT INTO meter_attempt (id, run_id, account_id, provider, request_started_at, policy_snapshot_id, due_at, due_reason, provider_contract_id, meter_semantics_id)
                VALUES ($ATTEMPT_ID, 1, 2, '\''provider-a'\'', $START_NS, 2, $START_NS, '\''ordinary_cadence'\'', '\''schema-v1'\'', '\''sem-v1'\'');
                INSERT INTO meter_attempt_result (attempt_id, completed_at, elapsed_nanos, outcome, clock_anomaly)
                VALUES ($ATTEMPT_ID, $FINISH_NS, 30000000000, '\''auth_required'\'', 0);
            "
        done
    ' _ "$LEDGER_DB"

    step "coverage human report" aub_coverage
    step "coverage json report" aub_coverage --format json
    step "coverage account selector" aub_coverage --account cred-failing
}

case_assertions() {
    assert_exit 6 1
    assert_stderr_contains 1 "no ledger exists"

    # Step 2: ledger initialized
    assert_exit 0 2

    # Step 3: seeding succeeded
    assert_exit 0 3

    # Step 4: human report exits 7 (threshold breached), shows both accounts
    # and distinguishes dead scheduler from credential failure
    assert_exit 7 4
    assert_stdout_contains 4 "coverage - last 24h"
    assert_stdout_contains 4 "dead-scheduler"
    assert_stdout_contains 4 "cred-failing"
    assert_stdout_contains 4 "scheduler ran normally"
    assert_stdout_contains 4 "attempts required authentication"
    assert_stderr_contains 4 "is below the 98% floor"
    assert_stderr_contains 4 "dead-scheduler"
    assert_stderr_contains 4 "cred-failing"

    # Step 5: JSON report exits 7 and carries versioned JSON structure with error envelope
    assert_exit 7 5
    assert_stdout_contains 5 '"command":"coverage"'
    assert_stdout_contains 5 '"schema":2'
    assert_stdout_contains 5 '"met":false'
    assert_json_field 5 "threshold.attempt_floor.value" "980000"
    assert_json_field 5 "threshold.measurement_floor.value" "950000"
    assert_json_field 5 "error.code" "THRESHOLD_NOT_MET"
    assert_json_field 5 "error.exit_class" "7"

    # Step 6: account selector isolates cred-failing
    assert_exit 7 6
    assert_stdout_contains 6 "cred-failing"
    assert_stdout_contains 6 "attempts required authentication"
    if grep -qF "dead-scheduler" "$(step_dir 6)/stdout.txt"; then
        record_assertion "account selector excludes unselected account" "absent" "present" "fail"
        CASE_FAILED=1
    else
        record_assertion "account selector excludes unselected account" "absent" "absent" "pass"
    fi
}
