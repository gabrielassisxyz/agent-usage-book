# aub-sth.6: the kill-between-stages proof of the two-stage meter attempt
# lifecycle, run against the release binary under the end-to-end runner, because
# the property is about a process rather than a function. `__attempt-crash-hook`
# is the documented crash-injection surface (PLAN.md section 34.7): its `start`
# stage commits the attempt start through the real store APIs and then aborts,
# so the case proves what survives a kill between the two commits, and its
# `complete` stage is the adjacent positive control that differs only in the
# crash injection.

CASE_ID="009-attempt-crash"
CASE_DESCRIPTION="A process killed between the attempt-start commit and the result write leaves exactly one started attempt with no terminal result; the complete control writes both facts."

case_preconditions() {
    require_command "$AUB_BIN"
}

case_steps() {
    step "crash after start commit" env \
        "AUB_STATE_DIR=$STATE_DIR/aub" \
        "$AUB_BIN" __attempt-crash-hook start
    step "read back what survived" env \
        "AUB_STATE_DIR=$STATE_DIR/aub" \
        "$AUB_BIN" __attempt-crash-hook read-back
    step "the permitted positive control" env \
        "AUB_STATE_DIR=$STATE_DIR/aub" \
        "$AUB_BIN" __attempt-crash-hook complete
    step "read back the control" env \
        "AUB_STATE_DIR=$STATE_DIR/aub" \
        "$AUB_BIN" __attempt-crash-hook read-back
}

case_assertions() {
    # The injected crash ends by signal: an ordinary exit here would mean the
    # hook committed and exited rather than injected the crash between stages.
    assert_signal 6 1

    # Exactly one start with no result survived the kill.
    assert_stdout_contains 2 "starts=1"
    assert_stdout_contains 2 "results=0"

    # The positive control exits cleanly and lands the second start with the
    # only result; nothing rewrote or removed the killed attempt.
    assert_exit 0 3
    assert_stdout_contains 4 "starts=2 results=1"
}