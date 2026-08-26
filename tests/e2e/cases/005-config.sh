# `aub config` against a seeded config file: every key is printed with the source
# that won, using a real file, a real environment override and a real command-line
# override in the same run.

CASE_ID="005-config"
CASE_DESCRIPTION="aub config prints every resolved key with the source that won, against a seeded file."

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
}

case_assertions() {
    assert_exit 0 1
    # Command-line override: highest precedence.
    assert_stdout_contains 1 "state.dir                        flag"
    # File: set in the seeded file, no flag or environment override present for it.
    assert_stdout_contains 1 "sampling.default_interval        file"
    assert_stdout_contains 1 "coverage.attempt_floor           file"
    # Environment: set via AUB_SAMPLING_REQUEST_TIMEOUT, no flag for it.
    assert_stdout_contains 1 "sampling.request_timeout         environment"
    # Default: nothing else was set for this key.
    assert_stdout_contains 1 "sampling.scheduler_tick          default"
}
