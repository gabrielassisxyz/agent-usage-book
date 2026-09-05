# aub-mgv.5: neither meter movement nor liveness evidence alone ever
# substitutes for an explicit marker. This case seeds a fresh heartbeat with
# no marker at all: a naive implementation that treats "the session is alive"
# as sufficient proof of "the session is spending under some account" would
# report an account here where none was ever named. Run against the release
# binary for the same reason 022 and 023 are.

CASE_ID="024-now-delayed-movement-without-marker"
CASE_DESCRIPTION="A fresh heartbeat with no explicit marker never becomes active-session evidence, whatever the meter is doing."

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
    # 1. Bootstrap: migrates the ledger and forces a meter attempt (the
    #    endpoint is unreachable, but this is the same "meter did something"
    #    step every other now-against-the-synthetic-provider case runs).
    step "bootstrap" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "AUB_ANTHROPIC_ENDPOINT=http://127.0.0.1:9" \
        "$AUB_BIN" now --account work-primary

    # 2. Seed a heartbeat 1 second old for a session with no marker at all.
    step "seed-heartbeat-only" sqlite3 "$LEDGER_DB" "
        INSERT INTO session_heartbeat
            (session_source, session_native, last_heartbeat_at, heartbeat_source)
        VALUES
            ('claude-code', 'sess-delayed-1', $((NOW_NS - 1000000000)), 'turn_end');
    "

    # 3. A live session with no marker still reports no_evidence: liveness
    #    alone proves nothing about which account, or that one applies at all.
    step "now-heartbeat-without-marker" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "AUB_ANTHROPIC_ENDPOINT=http://127.0.0.1:9" \
        "$AUB_BIN" now --account work-primary --session-id "claude-code:sess-delayed-1" --format json

    # 4. A second forced sample (more "meter movement") still changes nothing
    #    about the activity claim: it is composed only from markers and
    #    heartbeats, never from meter attempts.
    step "now-after-more-movement" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "AUB_ANTHROPIC_ENDPOINT=http://127.0.0.1:9" \
        "$AUB_BIN" now --account work-primary --session-id "claude-code:sess-delayed-1" --format json
}

case_assertions() {
    assert_exit 0 1
    assert_exit 0 2
    assert_exit 0 3
    assert_stdout_contains 3 '"activity":{"state":"no_evidence"}'
    assert_exit 0 4
    assert_stdout_contains 4 '"activity":{"state":"no_evidence"}'
}
