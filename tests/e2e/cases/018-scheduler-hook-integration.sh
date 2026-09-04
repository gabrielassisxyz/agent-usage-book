# The documented hook invocation from docs/scheduling.md (aub-eun.9):
# `aub sample --account ACCOUNT --if-due --session-id SESSION --run-id RUN`.
# Records the session/account marker, including its run-id join key, without
# a second network attempt when the account was recently sampled.

CASE_ID="018-scheduler-hook-integration"
CASE_DESCRIPTION="The documented hook invocation records a session/account marker with its run linkage even when no poll is due."

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
    # 1. Seed one prior attempt. An account with no history at all is due
    # regardless of --if-due (src/meter/due.rs rule 4), so the hook step below
    # would reach the network on a genuinely fresh account; this establishes
    # the "recently sampled" state the hook is meant to run against.
    step "seed-prior-attempt" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "AUB_ANTHROPIC_ENDPOINT=http://127.0.0.1:9" \
        "$AUB_BIN" sample --due

    # 2. The documented hook invocation, fired moments later within the same
    # cadence window: must record the marker, including --run-id, without a
    # second network attempt.
    step "hook-invocation" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "AUB_ANTHROPIC_ENDPOINT=http://127.0.0.1:9" \
        "$AUB_BIN" sample --account work-primary --if-due --session-id "cli:sess-hook-1" --run-id "cli:run-hook-1"

    # 3. Assert the marker and its run linkage landed in SQLite.
    step "query-marker" sqlite3 "$LEDGER_DB" \
        "SELECT session_native, run_native, evidence_designation FROM session_account_marker WHERE session_native = 'sess-hook-1'"
}

case_assertions() {
    # Step 1: seeding attempt exits 0 even against the unreachable endpoint.
    assert_exit 0 1
    assert_stdout_contains 1 "sample: account=work-primary outcome=unreachable"

    # Step 2: not due, so no second network attempt; the marker is still
    # recorded. If the implementation ignored --if-due and always sampled,
    # this would print "outcome=unreachable" instead of "not-due".
    assert_exit 0 2
    assert_stdout_contains 2 "sample: account=work-primary not-due"

    # Step 3: marker carries both the session and the run-id, and is tagged
    # as explicit launcher/hook evidence.
    assert_exit 0 3
    assert_stdout_contains 3 "sess-hook-1|run-hook-1|launcher_or_hook"
}
