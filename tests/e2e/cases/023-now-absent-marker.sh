# aub-mgv.5: `aub status --session-id` never claims a session is spending when no explicit
# marker exists for it, whether or not a session was even named. Run against
# the release binary for the same reason 022 is: the property is about what a
# real process reads back from a real ledger.

CASE_ID="023-now-absent-marker"
CASE_DESCRIPTION="With no explicit marker recorded, aub status reports no_evidence for a named session and omits activity when none is named."

case_preconditions() {
    require_command "$AUB_BIN"

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
    # 1. Bootstrap the projection so the human report has account rows.
    step "bootstrap-projection" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "AUB_ANTHROPIC_ENDPOINT=http://127.0.0.1:9" \
        "$AUB_BIN" status --refresh --account work-primary

    # 2. A session is named, but the store holds no marker for it at all.
    step "status-named-session-no-marker" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "$AUB_BIN" status --account work-primary --session-id "claude-code:sess-absent" --format json

    # 3. No --session-id at all: no activity key is emitted.
    step "status-no-session-named" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "$AUB_BIN" status --account work-primary --format json

    # 4. The human-text form carries no "aub session:" line at all in either
    #    case: nothing to claim is nothing printed, not an empty claim.
    step "status-human-no-marker" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "$AUB_BIN" status --account work-primary --session-id "claude-code:sess-absent"
}

case_assertions() {
    assert_exit 0 1
    assert_exit 0 2
    assert_stdout_contains 2 '"activity":{"state":"no_evidence"}'

    assert_exit 0 3
    if grep -qF '"activity"' "$(step_dir 3)/stdout.bin"; then
        record_assertion "status without --session-id omits activity" "absent" "present" "fail"
        CASE_FAILED=1
    else
        record_assertion "status without --session-id omits activity" "absent" "absent" "pass"
    fi

    assert_exit 0 4
    assert_stdout_contains 4 "work-primary  anthropic"
    # No "aub session:" line anywhere: the property this bead names ("no
    # generated report in a non-explicit evidence state contains an active
    # session or account claim") holds for the rendered human text too.
    if grep -qF "aub session:" "$(step_dir 4)/stdout.bin"; then
        record_assertion "no activity line without explicit evidence" "absent" "present" "fail"
        CASE_FAILED=1
    else
        record_assertion "no activity line without explicit evidence" "absent" "absent" "pass"
    fi
}
