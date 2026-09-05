# aub-mgv.5: an account switch closes the prior marker interval and changes
# the reported account at the documented boundary, for the live activity
# claim exactly as it already does for historical account attribution
# (aub-mgv.1). Run against the release binary for the same reason 022-024 are.

CASE_ID="025-now-account-switch-boundary"
CASE_DESCRIPTION="aub now reports the post-switch account once its marker covers the report instant, never the account the session started under."

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
    # 1. Bootstrap: migrates the ledger.
    step "bootstrap" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "AUB_ANTHROPIC_ENDPOINT=http://127.0.0.1:9" \
        "$AUB_BIN" now --account work-primary

    # 2. The session starts under account-a 60 seconds ago, switches to
    #    account-b 30 seconds ago, and has a heartbeat 1 second old: fresh
    #    enough for either account to read as live, so only the marker
    #    timeline decides which one is reported.
    step "seed-switch-and-heartbeat" sqlite3 "$LEDGER_DB" "
        INSERT INTO session_account_marker
            (session_source, session_native, observed_at, source_ordering_key,
             logical_account, resolved_account_id, marker_source, run_source,
             run_native, evidence_designation)
        VALUES
            ('claude-code', 'sess-switch-1', $((NOW_NS - 60000000000)), NULL,
             'account-a', NULL, 'hook', NULL, NULL, 'launcher_or_hook');
        INSERT INTO session_account_marker
            (session_source, session_native, observed_at, source_ordering_key,
             logical_account, resolved_account_id, marker_source, run_source,
             run_native, evidence_designation)
        VALUES
            ('claude-code', 'sess-switch-1', $((NOW_NS - 30000000000)), NULL,
             'account-b', NULL, 'hook', NULL, NULL, 'launcher_or_hook');
        INSERT INTO session_heartbeat
            (session_source, session_native, last_heartbeat_at, heartbeat_source)
        VALUES
            ('claude-code', 'sess-switch-1', $((NOW_NS - 1000000000)), 'turn_end');
    "

    # 3. The report instant is now, well after the switch: account-b, never
    #    account-a.
    step "now-after-switch" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "AUB_ANTHROPIC_ENDPOINT=http://127.0.0.1:9" \
        "$AUB_BIN" now --account work-primary --session-id "claude-code:sess-switch-1" --format json
}

case_assertions() {
    assert_exit 0 1
    assert_exit 0 2
    assert_exit 0 3
    assert_stdout_contains 3 '"activity":{"state":"explicit_marker_evidence","account":"account-b"'
    if grep -qF '"account":"account-a"' "$(step_dir 3)/stdout.bin"; then
        record_assertion "prior account never revived past the switch" "absent" "present" "fail"
        CASE_FAILED=1
    else
        record_assertion "prior account never revived past the switch" "absent" "absent" "pass"
    fi
}
