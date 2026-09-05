# aub-mgv.5: `aub now` never claims a session is spending when no explicit
# marker exists for it, whether or not a session was even named. Run against
# the release binary for the same reason 022 is: the property is about what a
# real process reads back from a real ledger.

CASE_ID="023-now-absent-marker"
CASE_DESCRIPTION="With no explicit marker recorded, aub now reports no_evidence, both for a named session and for none at all."

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
    # 1. A session is named, but the store holds no marker for it at all.
    step "now-named-session-no-marker" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "AUB_ANTHROPIC_ENDPOINT=http://127.0.0.1:9" \
        "$AUB_BIN" now --account work-primary --session-id "claude-code:sess-absent" --format json

    # 2. No --session-id at all: nothing was named to evaluate, and the
    #    disposition is identical.
    step "now-no-session-named" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "AUB_ANTHROPIC_ENDPOINT=http://127.0.0.1:9" \
        "$AUB_BIN" now --account work-primary --format json

    # 3. The human-text form carries no "aub session:" line at all in either
    #    case: nothing to claim is nothing printed, not an empty claim.
    step "now-human-no-marker" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "AUB_ANTHROPIC_ENDPOINT=http://127.0.0.1:9" \
        "$AUB_BIN" now --account work-primary --session-id "claude-code:sess-absent"
}

case_assertions() {
    assert_exit 0 1
    assert_stdout_contains 1 '"activity":{"state":"no_evidence"}'

    assert_exit 0 2
    assert_stdout_contains 2 '"activity":{"state":"no_evidence"}'

    assert_exit 0 3
    assert_stdout_contains 3 "aub work-primary"
    # No "aub session:" line anywhere: the property this bead names ("no
    # generated report in a non-explicit evidence state contains an active
    # session or account claim") holds for the rendered human text too.
    if grep -qF "aub session:" "$(step_dir 3)/stdout.bin"; then
        record_assertion "no activity line without explicit evidence" "absent" "present" "fail"
        CASE_FAILED=1
    else
        record_assertion "no activity line without explicit evidence" "absent" "absent" "pass"
    fi
}
