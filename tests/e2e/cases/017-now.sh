# aub-h3lo: `aub now` remains a deprecated alias for `aub status --refresh`,
# run against the release binary because the properties under test are about a
# process: what it writes to the store before it exits, and what the alias
# writes to stdout and stderr.
#
# The endpoint is unreachable (loopback port 9), so the forced attempt records
# an `unreachable` result with no response-evidence capsule. The success path
# and strict structured-event order run under tests/now_command.rs against the
# synthetic server; the kill-at-request-start property of the shared two-stage
# lifecycle runs under 009-attempt-crash.

CASE_ID="017-now"
CASE_DESCRIPTION="aub now warns and delegates to status --refresh, publishes a projection a following status agrees with, and never offers an unrecorded-fetch flag."

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
    # 1. now against an unreachable endpoint: warns, forces an attempt, records the
    #    unreachable result, publishes a projection, renders the stale reading,
    #    exits 0. -v surfaces the structured run events on stderr.
    step "now-forces-attempt" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "AUB_ANTHROPIC_ENDPOINT=http://127.0.0.1:9" \
        "$AUB_BIN" -v now

    # 2. The store holds exactly one attempt and one terminal result, and no
    #    response-evidence capsule (there was no successful response to capture).
    step "store-attempt-rows" sqlite3 "$LEDGER_DB" \
        "SELECT (SELECT count(*) FROM meter_attempt) || ' ' || (SELECT count(*) FROM meter_attempt_result) || ' ' || (SELECT count(*) FROM meter_response_evidence)"
    step "store-result-outcome" sqlite3 "$LEDGER_DB" \
        "SELECT count(*) || '|' || outcome FROM meter_attempt_result GROUP BY outcome"

    # 3. An immediate status reads the published projection and reports the same
    #    reading and the same freshness.
    step "status-after-now" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "$AUB_BIN" status

    # 4. JSON output names the delegated status command and carries one
    #    freshness variant.
    step "now-json" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "AUB_ANTHROPIC_ENDPOINT=http://127.0.0.1:9" \
        "$AUB_BIN" now --format json

    # 5. --account scopes the forced sample to the named account through the
    #    deprecated alias.
    step "now-account" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "AUB_ANTHROPIC_ENDPOINT=http://127.0.0.1:9" \
        "$AUB_BIN" now --account work-primary

    # 6. An unknown account is a usage error.
    step "now-unknown-account" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "$AUB_BIN" now --account nonexistent

    # 7. There is no flag that fetches without recording: a bypass-shaped flag is
    #    an unknown option, never an accepted mode.
    step "now-no-bypass-flag" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "$AUB_BIN" now --no-record

    # 8. The alias and its replacement receive the same text arguments and
    #    render the same status report.
    step "now-alias-text" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "AUB_ANTHROPIC_ENDPOINT=http://127.0.0.1:9" \
        "$AUB_BIN" now --account work-primary

    step "status-refresh-text" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "AUB_ANTHROPIC_ENDPOINT=http://127.0.0.1:9" \
        "$AUB_BIN" status --refresh --account work-primary

    # 9. The session selector is forwarded too; both commands use the same
    # no-evidence fixture and must produce the same status stdout and exit code.
    step "now-alias-session" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "AUB_ANTHROPIC_ENDPOINT=http://127.0.0.1:9" \
        "$AUB_BIN" now --account work-primary --session-id "claude-code:alias-session"

    step "status-refresh-session" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "AUB_ANTHROPIC_ENDPOINT=http://127.0.0.1:9" \
        "$AUB_BIN" status --refresh --account work-primary --session-id "claude-code:alias-session"
}

case_assertions() {
    # Step 1: now exits 0, renders the stale reading, emits the exact warning,
    # and the run events appear.
    assert_exit 0 1
    assert_stdout_contains 1 "work-primary  anthropic"
    assert_stdout_contains 1 "? · stale"
    assert_stderr_contains 1 "aub: 'now' is deprecated and will be removed; use 'aub status --refresh' instead"
    assert_stderr_contains 1 "run_started"
    assert_stderr_contains 1 "request_attempted"

    # Step 2: one attempt, one result, no evidence capsule for an unreachable.
    assert_exit 0 2
    assert_stdout_contains 2 "1 1 0"
    assert_exit 0 3
    assert_stdout_contains 3 "1|unreachable"

    # Step 3: status matches the alias-published reading and freshness.
    assert_exit 0 4
    assert_stdout_contains 4 "work-primary  anthropic"
    assert_stdout_contains 4 "? · stale"

    # Step 4: JSON names the delegated status command and the single freshness variant.
    assert_exit 0 5
    assert_stdout_contains 5 '"command":"status"'
    assert_stdout_contains 5 '"account":"work-primary"'
    assert_stdout_contains 5 '"freshness":"stale"'

    # Step 5: --account renders the named account.
    assert_exit 0 6
    assert_stdout_contains 6 "work-primary  anthropic"

    # Step 6: unknown account exits 2 (Usage).
    assert_exit 2 7
    assert_stderr_contains 7 "unknown account 'nonexistent'"

    # Step 8: a bypass-shaped flag is rejected, not honoured.
    assert_exit 2 8
    assert_stderr_contains 8 "unknown"

    assert_exit 0 9
    assert_exit 0 10
    if cmp -s "$(step_dir 9)/stdout.bin" "$(step_dir 10)/stdout.bin"; then
        record_assertion "now stdout equals status --refresh stdout" "identical" "identical" "pass"
    else
        record_assertion "now stdout equals status --refresh stdout" "identical" "different" "fail"
        CASE_FAILED=1
    fi

    assert_exit 0 11
    assert_stderr_contains 11 "aub: 'now' is deprecated and will be removed; use 'aub status --refresh' instead"
    assert_exit 0 12
    if cmp -s "$(step_dir 11)/stdout.bin" "$(step_dir 12)/stdout.bin"; then
        record_assertion "now session stdout equals status --refresh session stdout" "identical" "identical" "pass"
    else
        record_assertion "now session stdout equals status --refresh session stdout" "identical" "different" "fail"
        CASE_FAILED=1
    fi
}
