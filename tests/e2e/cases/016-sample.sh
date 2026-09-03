# `aub sample` observes provider endpoints for due or selected accounts,
# recording session markers and evidence with durable attempt tracking.

CASE_ID="016-sample"
CASE_DESCRIPTION="aub sample records markers and attempt evidence with appropriate exit status across scheduled and require-success modes."

LEDGER_DB=""

case_preconditions() {
    require_command "$AUB_BIN"
    require_command sqlite3

    LEDGER_DB="$STATE_DIR/ledger.db"

    mkdir -p "$STATE_DIR/home" "$STATE_DIR/creds"
    echo '{"accessToken":"test-token"}' > "$STATE_DIR/creds/token.json"

    cat > "$STATE_DIR/aub.toml" <<CFG_EOF
state.dir = "$STATE_DIR"

[[accounts]]
name = "work-primary"
provider = "anthropic"
credential = { kind = "file", path = "$STATE_DIR/creds/token.json" }
CFG_EOF
}

case_steps() {
    # 1. Unreachable endpoint in scheduled timer mode: records attempt and unreachable result, exits 0
    step "sample-due-unreachable-timer-exits-zero" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "AUB_ANTHROPIC_ENDPOINT=http://127.0.0.1:9" \
        "$AUB_BIN" sample --due

    # 2. Assert SQLite has attempt and unreachable result
    step "query-unreachable-evidence" sqlite3 "$LEDGER_DB" "SELECT count(*), outcome FROM meter_attempt_result GROUP BY outcome"

    # 3. Require-success with forced account against unreachable endpoint: records evidence and exits 4 (RemoteUnavailable)
    step "sample-account-require-success-exits-four" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "AUB_ANTHROPIC_ENDPOINT=http://127.0.0.1:9" \
        "$AUB_BIN" sample --account work-primary --require-success

    # 4. Marker recording with --if-due when not due: records marker to SQLite and skips network
    step "sample-if-due-with-marker" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "$AUB_BIN" sample --account work-primary --if-due --session-id "cli:test-sess-1"

    # 5. Assert marker exists in SQLite
    step "query-marker" sqlite3 "$LEDGER_DB" "SELECT session_native, logical_account FROM session_account_marker WHERE session_native = 'test-sess-1'"

    # 6. Usage error: bare aub sample without selector exits 2
    step "sample-usage-error" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "$AUB_BIN" sample
}

case_assertions() {
    # Step 1: scheduled --due exits 0 even on transport failure
    assert_exit 0 1
    assert_stdout_contains 1 "sample: account=work-primary outcome=unreachable"

    # Step 2: attempt result recorded
    assert_exit 0 2
    assert_stdout_contains 2 "1|unreachable"

    # Step 3: --require-success exits 4 on unreachable
    assert_exit 4 3
    assert_stdout_contains 3 "sample: account=work-primary outcome=unreachable"

    # Step 4: --if-due records marker and skips sampling (account is not due)
    assert_exit 0 4
    assert_stdout_contains 4 "sample: account=work-primary not-due"

    # Step 5: marker is in SQLite
    assert_exit 0 5
    assert_stdout_contains 5 "test-sess-1|work-primary"

    # Step 6: bare sample exits 2
    assert_exit 2 6
    assert_stderr_contains 6 "sample requires --due, --account, or --all"
}
