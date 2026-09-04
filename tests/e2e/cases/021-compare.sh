# aub-x2bq: aub compare records an adapter-semantics comparison through the
# release binary against a seeded observation, offers the still-uncompared
# windows before recording and refuses a second comparison for the same
# window after; aub doctor then names the comparison's age instead of
# reporting that none exists.

CASE_ID="021-compare"
CASE_DESCRIPTION="aub compare records a comparison through the binary, lists uncompared windows, refuses a duplicate, and aub doctor reports the comparison age."

LEDGER_DB=""
OBS_ID=""

case_preconditions() {
    require_command "$AUB_BIN"
    require_command sqlite3
    LEDGER_DB="$STATE_DIR/ledger.db"
}

case_steps() {
    step "seed one real meter observation" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "$AUB_BIN" __attempt-crash-hook sample --attempts 1

    OBS_ID="$(sqlite3 "$LEDGER_DB" "SELECT id FROM meter_observation ORDER BY id DESC LIMIT 1")"

    step "uncompared before recording names the five_hour window" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "$AUB_BIN" compare uncompared "$OBS_ID"

    step "record a comparison through the binary" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "$AUB_BIN" compare record "$OBS_ID" five_hour \
        --surface "anthropic console" \
        --surface-percent 25 \
        --granularity-percent 1 \
        --read-at "2026-09-04T12:00:00Z"

    step "uncompared after recording is empty" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "$AUB_BIN" compare uncompared "$OBS_ID"

    step "a second comparison for the same window is refused" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "$AUB_BIN" compare record "$OBS_ID" five_hour \
        --surface "anthropic console" \
        --surface-percent 25 \
        --granularity-percent 1 \
        --read-at "2026-09-04T12:00:00Z"

    step "doctor names the comparison age instead of reporting none exists" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "$AUB_BIN" doctor
}

case_assertions() {
    # Step 1: the fixture attempt lands as a real observation with one window.
    assert_exit 0 1

    # Step 2: before recording, the seeded five_hour window is uncompared.
    assert_exit 0 2
    assert_stdout_contains 2 "compare: observation=$OBS_ID uncompared window=five_hour"

    # Step 3: the verdict is computed, not accepted; the fixture's adapter
    # reading and the surface value given here agree exactly.
    assert_exit 0 3
    assert_stdout_contains 3 "compare: recorded observation=$OBS_ID window=five_hour"
    assert_stdout_contains 3 "verdict=agrees_within_granularity"

    # Step 4: the window just compared no longer shows up as uncompared.
    assert_exit 0 4
    assert_stdout_contains 4 "compare: observation $OBS_ID has no uncompared windows"

    # Step 5: a duplicate comparison for the same window is refused, naming
    # the existing comparison rather than writing a second record.
    assert_exit 2 5
    assert_stderr_contains 5 "already carries comparison"

    # Step 6: doctor reports the comparison's age instead of "none exists",
    # covering the command surface entry (aub-x2bq's own acceptance criterion).
    assert_exit 0 6
    assert_stdout_contains 6 "[PASS] adapter-semantics-comparison-age"
}
