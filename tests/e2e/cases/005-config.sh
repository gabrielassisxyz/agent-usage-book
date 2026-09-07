# `aub config` against a seeded config file: every key is printed with its
# resolved value and the source that won, in three aligned columns, using a
# real file, a real environment override and a real command-line override in
# the same run.

CASE_ID="005-config"
CASE_DESCRIPTION="aub config prints every resolved key with its value and the source that won, against a seeded file."

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
        "AUB_SAMPLING_REQUEST_TIMEOUT=9s" \
        "$AUB_BIN" config --set state.dir=/explicit/state/dir
    step "config with freshness override" env \
        "HOME=$STATE_DIR/home" \
        "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "AUB_SAMPLING_REQUEST_TIMEOUT=9s" \
        "$AUB_BIN" config --set freshness.meter=5m
}

case_assertions() {
    assert_exit 0 1
    # Command-line override: highest precedence, source `override`. The
    # overridden state.dir is the longest value in this run, so the value
    # column is fixed here and the whole line is pinned exactly.
    assert_stdout_contains 1 "state.dir                             /explicit/state/dir  override"
    # File: set in the seeded file, no flag or environment override present for it.
    assert_stdout_contains 1 "sampling.default_interval             3m                   file"
    assert_stdout_contains 1 "coverage.attempt_floor                0.9                  file"
    # Environment: set via AUB_SAMPLING_REQUEST_TIMEOUT, no flag for it.
    assert_stdout_contains 1 "sampling.request_timeout              9s                   environment"
    # Default: nothing else was set for this key.
    assert_stdout_contains 1 "sampling.scheduler_tick               1m                   default"
    # The longest key holds its source in the same column as every other row.
    assert_stdout_contains 1 "sampling.max_concurrent_requests      2                    default"
    # A --set override of a duration prints the value with source `override`
    # on the same row. The value column width moves with $STATE_DIR here
    # (the default state.dir carries it), so this matches the row shape
    # rather than exact spacing.
    assert_exit 0 2
    local override_line
    override_line="$(grep "^freshness.meter " "$(step_dir 2)/stdout.txt")"
    if printf '%s' "$override_line" | grep -Eq "^freshness\.meter +5m +override$"; then
        record_assertion "freshness override row" "freshness.meter 5m override" "$override_line" "pass"
    else
        record_assertion "freshness override row" "freshness.meter 5m override" "$override_line" "fail"
        CASE_FAILED=1
    fi
}
