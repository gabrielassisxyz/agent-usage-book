# aub-mgv.5: `aub now --session-id` claims a session is actively spending only
# when an explicit session/account marker AND a fresh heartbeat both cover the
# report instant, run against the release binary because the property under
# test is what a real process reads back from a real SQLite ledger, not one
# function call.
#
# The endpoint is unreachable (loopback port 9), matching every other `now`
# e2e case: the meter reading itself is irrelevant to this bead, whose
# correctness invariant is that activity evidence is composed independently of
# meter movement. The marker and heartbeat are seeded directly with sqlite3,
# the same way 018-scheduler-hook-integration.sh reads one back, because
# `aub` ships no command that only records a heartbeat.

CASE_ID="022-now-explicit-marker-liveness"
CASE_DESCRIPTION="An explicit marker with a fresh heartbeat makes aub now report the session as actively spending, in both human and JSON output."

LEDGER_DB=""
NOW_NS=""

case_preconditions() {
    require_command "$AUB_BIN"
    require_command sqlite3

    LEDGER_DB="$STATE_DIR/ledger.db"
    NOW_NS="$(date +%s%N)"

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
    # 1. Bootstrap: migrates the ledger and forces one attempt. No marker or
    #    heartbeat exists yet, so activity is no_evidence even with a session
    #    named.
    step "bootstrap-no-evidence" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "AUB_ANTHROPIC_ENDPOINT=http://127.0.0.1:9" \
        "$AUB_BIN" -v now --account work-primary --session-id "claude-code:sess-explicit-1" --format json

    # 2. Seed an explicit marker 5 seconds old and a heartbeat 1 second old:
    #    both comfortably within the 15-minute default liveness horizon.
    step "seed-marker-and-heartbeat" sqlite3 "$LEDGER_DB" "
        INSERT INTO session_account_marker
            (session_source, session_native, observed_at, source_ordering_key,
             logical_account, resolved_account_id, marker_source, run_source,
             run_native, evidence_designation)
        VALUES
            ('claude-code', 'sess-explicit-1', $((NOW_NS - 5000000000)), NULL,
             'work-primary', NULL, 'hook', NULL, NULL, 'launcher_or_hook');
        INSERT INTO session_heartbeat
            (session_source, session_native, last_heartbeat_at, heartbeat_source)
        VALUES
            ('claude-code', 'sess-explicit-1', $((NOW_NS - 1000000000)), 'turn_end');
    "

    # 3. Now the same session reports as spending, in human text.
    step "now-spending-human" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "AUB_ANTHROPIC_ENDPOINT=http://127.0.0.1:9" \
        "$AUB_BIN" now --account work-primary --session-id "claude-code:sess-explicit-1"

    # 4. And in versioned JSON, carrying the evidence class and both
    #    provenance identifiers.
    step "now-spending-json" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "AUB_ANTHROPIC_ENDPOINT=http://127.0.0.1:9" \
        "$AUB_BIN" now --account work-primary --session-id "claude-code:sess-explicit-1" --format json
}

case_assertions() {
    # Step 1: no marker, no heartbeat yet: no_evidence, and no "spending" claim
    # anywhere in the JSON.
    assert_exit 0 1
    assert_stdout_contains 1 '"activity":{"state":"no_evidence"}'
    assert_stderr_contains 1 "report_rendered"

    # Step 2: seeding succeeds.
    assert_exit 0 2

    # Step 3: human text now names the account, the marker and the heartbeat.
    assert_exit 0 3
    assert_stdout_contains 3 "aub session: spending account=work-primary marker=session_account_marker:1 heartbeat=session_heartbeat:1"

    # Step 4: JSON carries the same evidence class and both provenance
    # identifiers.
    assert_exit 0 4
    assert_stdout_contains 4 '"command":"now"'
    assert_stdout_contains 4 '"activity":{"state":"explicit_marker_evidence","account":"work-primary","marker":"session_account_marker:1","heartbeat":"session_heartbeat:1"}'
}
