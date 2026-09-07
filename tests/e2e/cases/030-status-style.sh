# The style layer's contract, exercised through the release binary: colour
# appears only when stdout is a terminal and nothing suppressed it; --no-color,
# NO_COLOR and a pipe each render plain; the three plain forms are
# byte-identical; and JSON never carries an escape in any of the three modes.

CASE_ID="030-status-style"
CASE_DESCRIPTION="status colour tracks the tty, NO_COLOR and --no-color, and JSON is never styled."

CONFIG_FILE=""

# The 24-bit colour escape prefix the style layer emits, the tone escape of the
# seeded fresh reading (620000 ppm used leaves 380000 ppm, 38%, which is
# yellow), and the single escape byte any stray styling would carry. Spelled
# with $'...' so the case file itself stays ASCII.
ESCAPE_PREFIX=$'\x1b[38;2;'
YELLOW_ESCAPE=$'\x1b[38;2;224;175;104m'
ESCAPE_ANY=$'\x1b'

case_preconditions() {
    require_command script
    require_command stty
    CONFIG_FILE="$STATE_DIR/aub.toml"
    cat > "$CONFIG_FILE" <<EOT
state.dir = "$STATE_DIR/state"

[[accounts]]
name = "work-primary"
provider = "provider-a"
EOT
    mkdir -p "$STATE_DIR/state"

    # One fresh observation, observed 41 seconds ago, so the account line
    # carries a remaining fraction for the tone to follow.
    now="$(date +%s%N)"
    received="$((now - 41 * 1000000000))"
    cat > "$STATE_DIR/state/projection" <<EOT
{"schema_version":2,"ledger_generation":12,"accounts":[{"account_id":1,"logical_name":"work-primary","provider":"provider-a","last_successful_observation":{"observation_id":7,"provider_observed_at_nanos":${received},"received_at_nanos":${received},"measurement_basis":"provider_observed","provider_contract_id":"contract-v1","windows":[{"semantic_key":"five_hour","scope_kind":"account_wide","scoped_model":null,"quota_used_ppm":620000,"reported_resolution_ppm":10000,"quantization":"exact","resets_at_nanos":$((now + 3 * 3600 * 1000000000)),"nominal_duration_nanos":18000000000000,"is_active":true,"severity":"unknown"}]},"latest_attempt":{"attempt_id":9,"request_started_at_nanos":${received},"credential_context_id":"ctx","result":{"completed_at_nanos":${received},"outcome":"success","failure_class":null}}}]}
EOT
}

case_steps() {
    # Step 1: a pty with NO_COLOR unset, the one mode colour is on in. The pty
    # runs with -opost so the relay carries the child's own line endings
    # rather than the line discipline's, and script -e returns the child's own
    # exit status, which script without it always replaces with zero.
    step "colour under pty" \
        env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG_FILE" \
        script -q -e -c "stty -opost; '$AUB_BIN' status" /dev/null

    # Steps 2 and 3: the two plain forms under the same pty, one by flag and
    # one by environment.
    step "plain by flag under pty" \
        env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG_FILE" \
        script -q -e -c "stty -opost; '$AUB_BIN' status --no-color" /dev/null
    step "plain by environment under pty" \
        env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG_FILE" "NO_COLOR=1" \
        script -q -e -c "stty -opost; '$AUB_BIN' status" /dev/null

    # Step 4: a pipe, with the real exit status visible to the runner because
    # the step's argv is the aub invocation itself.
    step "plain through a pipe" \
        env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG_FILE" "$AUB_BIN" status

    # Step 5: piped through cat, the third plain form the byte-identity
    # criterion names.
    step "plain piped through cat" \
        sh -c "env HOME=$STATE_DIR/home AUB_CONFIG_FILE=$CONFIG_FILE '$AUB_BIN' status | cat"

    # Steps 6 to 8: JSON in the three modes, none of which may carry styling.
    step "json under pty" \
        env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG_FILE" \
        script -q -e -c "stty -opost; '$AUB_BIN' status --format json" /dev/null
    step "json plain by flag under pty" \
        env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG_FILE" \
        script -q -e -c "stty -opost; '$AUB_BIN' status --format json --no-color" /dev/null
    step "json plain by environment under pty" \
        env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG_FILE" "NO_COLOR=1" \
        script -q -e -c "stty -opost; '$AUB_BIN' status --format json" /dev/null
}

case_assertions() {
    # Every status invocation exits zero, whatever the mode. The script steps
    # report the child's own status through -e.
    for n in 1 2 3 4 5 6 7 8; do
        assert_exit 0 "$n"
    done

    # Colour under a pty: a 24-bit foreground escape is present, it tints the
    # fresh reading, and the account line still names the account. The words
    # underneath the tint are pinned by the render tests; here the pty is
    # what is under test.
    assert_stdout_contains 1 "$ESCAPE_PREFIX"
    assert_stdout_contains 1 "${YELLOW_ESCAPE}38% left"
    assert_stdout_contains 1 "aub work-primary"

    # The two plain pty forms and the two pipe forms carry no escape at all.
    for n in 2 3 4 5; do
        assert_stdout_lacks "$n" "$ESCAPE_ANY" "no-escape"
    done
    # The flag form is byte-identical to the environment form and to the form
    # piped through cat: three spellings, one output.
    assert_stdout_identical 2 3
    assert_stdout_identical 2 5

    # JSON in all three modes: no escape byte anywhere, and still the status
    # envelope.
    for n in 6 7 8; do
        assert_stdout_lacks "$n" "$ESCAPE_ANY" "no-escape"
        assert_json_field "$n" command status
    done
}

# assert_stdout_lacks STEP NEEDLE LABEL: the step's stdout does not contain
# NEEDLE, with LABEL naming what the absence means in the assertion log.
assert_stdout_lacks() {
    local step="$1" needle="$2" label="$3"
    if grep -qF -- "$needle" "$(step_dir "$step")/stdout.bin"; then
        record_assertion "assert_stdout_lacks step $step" "$label" "present" "fail"
        CASE_FAILED=1
    else
        record_assertion "assert_stdout_lacks step $step" "$label" "absent" "pass"
    fi
}

# assert_stdout_identical STEP_A STEP_B: the two steps' stdout artifacts are
# byte for byte the same, which is what the plain-form identity criterion asks.
assert_stdout_identical() {
    local a="$1" b="$2"
    if cmp -s "$(step_dir "$a")/stdout.bin" "$(step_dir "$b")/stdout.bin"; then
        record_assertion "assert_stdout_identical steps $a=$b" "identical" "identical" "pass"
    else
        record_assertion "assert_stdout_identical steps $a=$b" "identical" "differ" "fail"
        CASE_FAILED=1
    fi
}