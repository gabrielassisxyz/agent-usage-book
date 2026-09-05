# aub-c0b.9: `aub calibrate show|history|compare|activate` replace the
# singleton coefficient command, showing the active calibration together with
# everything needed to judge it.
#
# Seeding goes through the real store functions behind the `__calibration-fixture`
# test hook rather than hand-writing SQL against schemas this file does not own
# (the `026-can-run.sh` convention). The fixture records one active calibration
# (`five_hour-fixture-calibration`, fitted 500000 micros/point) through the real
# experiment/result/activation chain, so `show` has an active record, `history`
# has an activation event, `compare` has a record to name twice, and `activate`
# has a record whose evidence the refusal steps can misstate.

CASE_ID="027-calibrate-show-history-compare-activate"
CASE_DESCRIPTION="aub calibrate show, history, compare and activate render the active calibration with residual and uncertainty, list lifecycle events, report percentage differences, and refuse activations that miss the policy, all through the release binary."

LEDGER_DB=""
CAL_ID="five_hour-fixture-calibration"

case_preconditions() {
    require_command "$AUB_BIN"
    require_command sqlite3
    LEDGER_DB="$STATE_DIR/ledger.db"

    mkdir -p "$STATE_DIR/home"
    cat > "$STATE_DIR/aub.toml" <<CFG_EOF
state.dir = "$STATE_DIR"
CFG_EOF
}

case_steps() {
    # 1-2. Empty ledger: show and history say so rather than failing.
    step "calibrate-show-empty" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "$AUB_BIN" calibrate show

    step "calibrate-history-empty" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "$AUB_BIN" calibrate history

    # 3. Seed one active calibration through the real chain.
    step "seed-fixture-calibration" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "$AUB_BIN" __calibration-fixture five_hour 500000

    # 4-5. Show the active calibration as text and JSON.
    step "calibrate-show" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "$AUB_BIN" calibrate show

    step "calibrate-show-json" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "$AUB_BIN" calibrate show --format json

    # 6-7. History as text and JSON.
    step "calibrate-history" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "$AUB_BIN" calibrate history

    step "calibrate-history-json" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "$AUB_BIN" calibrate history --format json

    # 8-9. Compare the record against itself: zero difference, active.
    step "calibrate-compare-self" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "$AUB_BIN" calibrate compare "$CAL_ID" "$CAL_ID"

    step "calibrate-compare-json" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "$AUB_BIN" calibrate compare "$CAL_ID" "$CAL_ID" --format json

    # 10. Compare against a calibration that does not exist: usage, not a number.
    step "calibrate-compare-unknown" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "$AUB_BIN" calibrate compare "$CAL_ID" no-such-calibration

    # 11. Activate with substitute evidence: refused with the reason.
    step "calibrate-activate-wrong-evidence" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "$AUB_BIN" calibrate activate "$CAL_ID" \
        --training substitute-1 --validation substitute-2

    # 12. Activate without naming the evidence: usage, not a silent act.
    step "calibrate-activate-missing-evidence" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "$AUB_BIN" calibrate activate "$CAL_ID"
}

case_assertions() {
    assert_exit 0 1
    assert_stdout_contains 1 "no active calibration"

    assert_exit 0 2
    assert_stdout_contains 2 "no calibrations recorded"

    assert_exit 0 3
    assert_stdout_contains 3 "calibration $CAL_ID active for window five_hour"

    # Step 4: the active coefficient with residual, uncertainty and health.
    assert_exit 0 4
    assert_stdout_contains 4 "active window calibration $CAL_ID"
    assert_stdout_contains 4 "fitted:          500000 micros/point"
    assert_stdout_contains 4 "uncertainty:"
    assert_stdout_contains 4 "residual:"
    assert_stdout_contains 4 "health:          current"
    assert_stdout_contains 4 "unknown kinds  none"

    # Step 5: JSON carries units and provenance identifiers.
    assert_exit 0 5
    assert_json_field 5 command calibrate-show
    assert_stdout_contains 5 '"unit":"micros_per_point"'
    assert_stdout_contains 5 '"unit":"credits"'
    assert_stdout_contains 5 "$CAL_ID"

    # Step 6: history names the activation event and the health state.
    assert_exit 0 6
    assert_stdout_contains 6 "calibration $CAL_ID (current)"
    assert_stdout_contains 6 "activation"
    assert_stdout_contains 6 "fixture"

    # Step 7: history JSON carries the event trail.
    assert_exit 0 7
    assert_json_field 7 command calibrate-history
    assert_stdout_contains 7 '"kind":"activation"'

    # Step 8: a zero self-difference that states the active status.
    assert_exit 0 8
    assert_stdout_contains 8 "candidate $CAL_ID differs from active $CAL_ID by 0.0%"
    assert_stdout_contains 8 "candidate is active"

    # Step 9: compare JSON carries the difference and both coefficients.
    assert_exit 0 9
    assert_json_field 9 command calibrate-compare
    assert_stdout_contains 9 '"difference_bps":0'
    assert_stdout_contains 9 '"candidate_is_active":true'

    # Step 10: an unknown id is a usage error naming the id.
    assert_exit 2 10
    assert_stderr_contains 10 "no calibration 'no-such-calibration'"

    # Step 11: substitute evidence is refused with the reason (exit 7).
    assert_exit 7 11
    assert_stderr_contains 11 "does not reproduce"

    # Step 12: unnamed evidence is a usage error naming the flag.
    assert_exit 2 12
    assert_stderr_contains 12 "--training"

    # No refused activation wrote anything: the one fixture event stands alone.
    local lifecycle_count
    lifecycle_count="$(sqlite3 "$LEDGER_DB" "SELECT COUNT(*) FROM calibration_lifecycle")"
    if [ "$lifecycle_count" -ne 1 ]; then
        echo "refused activations must write nothing: calibration_lifecycle has $lifecycle_count rows (must be 1)" >&2
        return 1
    fi
}
