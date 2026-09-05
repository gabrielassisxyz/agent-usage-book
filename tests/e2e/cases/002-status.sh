# The status command reads the projection and nothing else: the degraded
# question mark when the projection is missing, the never-observed rendering
# for an account the projection says nothing about, and a structured
# diagnostic run event on stderr at raised verbosity.

CASE_ID="002-status"
CASE_DESCRIPTION="status renders the projection or the degraded question mark, and never blocks."

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

case_steps() {
    step "status without projection" \
        env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG_FILE" "$AUB_BIN" status
    step "status verbose" \
        env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG_FILE" "$AUB_BIN" -v status
    # The case seeds a projection for an account with no observation and no
    # attempt: the state on disk is then the fixture the third step reads, and
    # the step digests record exactly when it appeared.
    cat > "$STATE_DIR/state/projection" <<EOT
{"schema_version":2,"ledger_generation":12,"accounts":[{"account_id":1,"logical_name":"work-primary","provider":"provider-a","last_successful_observation":null,"latest_attempt":null}]}
EOT
    step "status with projection" \
        env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG_FILE" "$AUB_BIN" status
}

case_assertions() {
    # Missing projection: exit zero, the degraded question mark, no account
    # value substituted for the readings that cannot exist.
    assert_exit 0 1
    assert_golden 1 "$REPO_ROOT/tests/e2e/status-missing-projection.txt"
    assert_stdout_equals 1 "aub ?"
    assert_exit 0 2
    assert_stderr_contains 2 "run_started"
    assert_exit 0 3
    assert_stdout_contains 3 "aub work-primary ? · stale · no successful sample"
}
