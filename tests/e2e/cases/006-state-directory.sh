# aub-sth.2: a mutating command refuses to begin against a state directory it cannot
# write to. `__state-check` is the test-only surface that runs this project's real
# state-directory readiness check ahead of a stand-in network call; the case proves
# both that the refusal exits with the store-failure class and that the
# request-attempted event never reaches the structured log, which is what proves the
# refusal happened before any network access rather than merely alongside it.

CASE_ID="006-state-directory"
CASE_DESCRIPTION="__state-check refuses an unwritable state directory before a request, and opens its state database at mode 0600 on the permitted path."

BLOCKED_STATE_DIR=""

case_preconditions() {
    # The parent, not the leaf, is made unwritable: the leaf's own mode is force-set
    # by this project's own directory-creation step (the correct, self-healing
    # response to "permissions wider than intended"), so a leaf-only restriction
    # would be silently repaired rather than refused. A parent with no execute bit
    # cannot be traversed by this process regardless of ownership, which is the
    # genuinely unrecoverable case this bead's refusal exists for.
    mkdir -p "$STATE_DIR/blocked"
    chmod 000 "$STATE_DIR/blocked"
    BLOCKED_STATE_DIR="$STATE_DIR/blocked/aub"
}

case_steps() {
    step "state-check" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$BLOCKED_STATE_DIR" \
        "$AUB_BIN" __state-check
    step "create secured state database" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR/ready" \
        "$AUB_BIN" __state-check
}

case_assertions() {
    assert_exit 5 1

    if grep -qF "request_attempted" "$(step_dir 1)/stderr.bin"; then
        record_assertion "no request_attempted event logged" "absent" "present" "fail"
        CASE_FAILED=1
    else
        record_assertion "no request_attempted event logged" "absent" "absent" "pass"
    fi

    assert_exit 0 2
    local database_mode
    database_mode="$(stat -c %a "$STATE_DIR/ready/state-check.db")"
    if [ "$database_mode" = "600" ]; then
        record_assertion "state database mode" "600" "$database_mode" "pass"
    else
        record_assertion "state database mode" "600" "$database_mode" "fail"
        CASE_FAILED=1
    fi

    # Restore removability before the run directory is pruned or inspected by hand;
    # a directory this test made unwritable must not be a permanent guard-lockout in
    # the repository's build output.
    chmod 700 "$STATE_DIR/blocked" 2>/dev/null || true
}
