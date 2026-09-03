# All six states `aub status` can display, rendered by the release binary
# against seeded projections: fresh, stale, auth required, no successful
# sample, collector interrupted and the missing projection, each exiting zero,
# with argument failures as the only non-zero exits.

CASE_ID="013-status-projection"
CASE_DESCRIPTION="the six status states render from seeded projections, and exit zero."

CONFIG_FILE=""

case_preconditions() {
    CONFIG_FILE="$STATE_DIR/aub.toml"
    cat > "$CONFIG_FILE" <<EOT
state.dir = "$STATE_DIR/state"

[[accounts]]
name = "work-primary"
provider = "provider-a"
EOT
    mkdir -p "$STATE_DIR/state"
}

# seed_projection OBSERVATION_JSON ATTEMPT_JSON: writes a schema-v1 projection
# with one account and the given observation and attempt records.
seed_projection() {
    local observation="$1" attempt="$2"
    cat > "$STATE_DIR/state/projection" <<EOT
{"schema_version":1,"ledger_generation":12,"accounts":[{"account_id":1,"logical_name":"work-primary","provider":"provider-a","last_successful_observation":${observation},"latest_attempt":${attempt}}]}
EOT
    echo "$now" > "$CASE_LOG_DIR/seeded-at-nanos.txt"
}

# window USED_PPM RECEIVED_NANOS: one account-wide five-hour window.
window() {
    local used_ppm="$1" received="$2"
    printf '{"semantic_key":"five_hour","scope_kind":"account_wide","scoped_model":null,"quota_used_ppm":%s,"reported_resolution_ppm":10000,"quantization":"exact","resets_at_nanos":%s,"nominal_duration_nanos":18000000000000}' \
        "$used_ppm" "$((now + 3 * 3600 * 1000000000))"
}

now="$(date +%s%N)"
case_steps() {
    # Step 1: fresh, observed 41 seconds ago, success 41 seconds ago.
    local received="$((now - 41 * 1000000000))"
    seed_projection \
        "{\"observation_id\":7,\"provider_observed_at_nanos\":${received},\"received_at_nanos\":${received},\"measurement_basis\":\"provider_observed\",\"windows\":[$(window 620000 "$received")]}" \
        "{\"attempt_id\":9,\"request_started_at_nanos\":${received},\"credential_context_id\":\"ctx\",\"result\":{\"completed_at_nanos\":${received},\"outcome\":\"success\",\"failure_class\":null}}"
    step "fresh" env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG_FILE" "$AUB_BIN" status

    # Step 2: stale by age, observed 14 minutes ago.
    received="$((now - 14 * 60 * 1000000000))"
    seed_projection \
        "{\"observation_id\":7,\"provider_observed_at_nanos\":${received},\"received_at_nanos\":${received},\"measurement_basis\":\"provider_observed\",\"windows\":[$(window 620000 "$received")]}" \
        "{\"attempt_id\":9,\"request_started_at_nanos\":${received},\"credential_context_id\":\"ctx\",\"result\":{\"completed_at_nanos\":${received},\"outcome\":\"success\",\"failure_class\":null}}"
    step "stale" env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG_FILE" "$AUB_BIN" status

    # Step 3: auth required.
    received="$((now - 5 * 60 * 1000000000))"
    seed_projection \
        "{\"observation_id\":7,\"provider_observed_at_nanos\":${received},\"received_at_nanos\":${received},\"measurement_basis\":\"provider_observed\",\"windows\":[$(window 620000 "$received")]}" \
        "{\"attempt_id\":9,\"request_started_at_nanos\":${received},\"credential_context_id\":\"ctx\",\"result\":{\"completed_at_nanos\":${received},\"outcome\":\"auth_required\",\"failure_class\":null}}"
    step "auth" env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG_FILE" "$AUB_BIN" status

    # Step 4: no successful sample ever.
    seed_projection "null" "null"
    step "no sample" env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG_FILE" "$AUB_BIN" status

    # Step 5: collector interrupted, an attempt started and never finished.
    received="$((now - 9 * 60 * 1000000000))"
    seed_projection \
        "{\"observation_id\":7,\"provider_observed_at_nanos\":${received},\"received_at_nanos\":${received},\"measurement_basis\":\"provider_observed\",\"windows\":[$(window 620000 "$received")]}" \
        "{\"attempt_id\":9,\"request_started_at_nanos\":${received},\"credential_context_id\":\"ctx\",\"result\":null}"
    step "interrupted" env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG_FILE" "$AUB_BIN" status

    # Step 6: missing projection, and step 7: an unsupported schema version.
    rm -f "$STATE_DIR/state/projection"
    step "missing" env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG_FILE" "$AUB_BIN" status
    printf '{"schema_version":99,"ledger_generation":1,"accounts":[]}' > "$STATE_DIR/state/projection"
    step "unsupported schema" env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG_FILE" "$AUB_BIN" status --explain

    # Step 8: default stderr is empty.
    step "quiet stderr" env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG_FILE" "$AUB_BIN" status

    # Steps 9 and 10: the only non-zero exits are argument failures.
    step "unknown flag" env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG_FILE" "$AUB_BIN" status --definitely-not-a-flag
    step "unknown account" env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG_FILE" "$AUB_BIN" status --account nobody-configured
}

case_assertions() {
    assert_exit 0 1
    assert_stdout_contains 1 "aub work-primary 38% left · 5h"

    assert_exit 0 2
    assert_stdout_contains 2 "aub work-primary ~38% · stale 14m · age exceeded"

    assert_exit 0 3
    assert_stdout_contains 3 "aub work-primary auth!"

    assert_exit 0 4
    assert_stdout_contains 4 "aub work-primary ? · stale · no successful sample"

    assert_exit 0 5
    assert_stdout_contains 5 "aub work-primary ~38% · stale 9m · collector interrupted"

    assert_exit 0 6
    assert_stdout_equals_declared_missing 6
    assert_exit 0 7
    assert_stdout_contains 7 "aub ?"
    assert_stdout_contains 7 "schema version 99"

    assert_exit 0 8
    assert_stderr_empty 8

    assert_exit 2 9
    assert_exit 2 10
    assert_stderr_contains 10 "unknown account 'nobody-configured'"
}

# assert_stderr_empty STEP: the step wrote nothing on stderr, which the status
# contract promises at the default diagnostic level.
assert_stderr_empty() {
    local step="$1"
    if [ ! -s "$(step_dir "$step")/stderr.bin" ]; then
        record_assertion "assert_stderr_empty step $step" "empty" "empty" "pass"
    else
        record_assertion "assert_stderr_empty step $step" "empty" "non-empty" "fail"
        CASE_FAILED=1
    fi
}

assert_stdout_equals_declared_missing() {
    local step="$1"
    assert_golden "$step" "$REPO_ROOT/tests/e2e/status-missing-projection.txt"
    assert_stdout_equals "$step" "aub ?"
}