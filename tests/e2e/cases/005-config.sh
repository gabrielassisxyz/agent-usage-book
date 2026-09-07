# `aub config` against a seeded config file: one box titled with the config
# file path, one block per section with the keys under it without the section
# prefix, using a real file, a real environment override and a real
# command-line override in the same run.

CASE_ID="005-config"
CASE_DESCRIPTION="aub config prints the boxed sectioned listing with every resolved key, its value and the source that won, against a seeded file."

CONFIG_FILE=""

case_preconditions() {
    CONFIG_FILE="$STATE_DIR/aub.toml"
    cat > "$CONFIG_FILE" <<'EOF'
[sampling]
default_interval = "3m"

[coverage]
attempt_floor = 0.9
EOF
}

case_steps() {
    step "config" env \
        "HOME=$STATE_DIR/home" \
        "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "$AUB_BIN" config --set state.dir=/explicit/state/dir
    step "config with freshness override" env \
        "HOME=$STATE_DIR/home" \
        "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "AUB_SAMPLING_REQUEST_TIMEOUT=9s" \
        "$AUB_BIN" config --set freshness.meter=5m
}

case_assertions() {
    assert_exit 0 1
    # The box frame with the config file path in the title.
    assert_stdout_contains 1 "config ·"
    assert_stdout_contains 1 "$CONFIG_FILE"
    # Sections print as bare titles, keys without the section prefix.
    assert_stdout_contains 1 "sampling"
    assert_stdout_contains 1 "coverage"
    assert_stdout_contains 1 "freshness"
    assert_stdout_contains 1 "default_interval"
    assert_stdout_contains 1 "max_concurrent_requests"
    # No dotted key survives the boxed layout.
    if grep -qF "sampling." "$(step_dir 1)/stdout.txt"; then
        record_assertion "no dotted keys" "no sampling. prefix" "dotted key present" "fail"
        CASE_FAILED=1
    else
        record_assertion "no dotted keys" "no sampling. prefix" "no sampling. prefix" "pass"
    fi
    # Command-line override: highest precedence, source `override`.
    if grep -Eq "dir +/explicit/state/dir +override" "$(step_dir 1)/stdout.txt"; then
        record_assertion "state override row" "dir /explicit/state/dir override" "row present" "pass"
    else
        record_assertion "state override row" "dir /explicit/state/dir override" "row absent" "fail"
        CASE_FAILED=1
    fi
    # File: set in the seeded file, no flag or environment override present for it.
    if grep -Eq "default_interval +3m +file" "$(step_dir 1)/stdout.txt"; then
        record_assertion "file row" "default_interval 3m file" "row present" "pass"
    else
        record_assertion "file row" "default_interval 3m file" "row absent" "fail"
        CASE_FAILED=1
    fi
    if grep -Eq "attempt_floor +0\.9 +file" "$(step_dir 1)/stdout.txt"; then
        record_assertion "coverage file row" "attempt_floor 0.9 file" "row present" "pass"
    else
        record_assertion "coverage file row" "attempt_floor 0.9 file" "row absent" "fail"
        CASE_FAILED=1
    fi
    # Default: nothing else was set for this key.
    if grep -Eq "scheduler_tick +1m +default" "$(step_dir 1)/stdout.txt"; then
        record_assertion "default row" "scheduler_tick 1m default" "row present" "pass"
    else
        record_assertion "default row" "scheduler_tick 1m default" "row absent" "fail"
        CASE_FAILED=1
    fi
    # The longest key holds its source in the same column as every other row.
    if grep -Eq "max_concurrent_requests +2 +default" "$(step_dir 1)/stdout.txt"; then
        record_assertion "longest key row" "max_concurrent_requests 2 default" "row present" "pass"
    else
        record_assertion "longest key row" "max_concurrent_requests 2 default" "row absent" "fail"
        CASE_FAILED=1
    fi
    # A --set override of a duration prints the value with source `override`
    # on the same row inside the box.
    assert_exit 0 2
    local override_line
    override_line="$(grep -E "meter +5m +override" "$(step_dir 2)/stdout.txt" | head -n 1)"
    if [ -n "$override_line" ]; then
        record_assertion "freshness override row" "meter 5m override" "$override_line" "pass"
    else
        record_assertion "freshness override row" "meter 5m override" "row absent" "fail"
        CASE_FAILED=1
    fi
    # Environment: set via AUB_SAMPLING_REQUEST_TIMEOUT, no flag for it.
    if grep -Eq "request_timeout +9s +environment" "$(step_dir 2)/stdout.txt"; then
        record_assertion "environment row" "request_timeout 9s environment" "row present" "pass"
    else
        record_assertion "environment row" "request_timeout 9s environment" "row absent" "fail"
        CASE_FAILED=1
    fi
}
