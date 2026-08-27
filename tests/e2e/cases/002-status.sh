# The status command exits zero, emits a structured diagnostic event on stderr at
# raised verbosity, and renders each configured account's meter reading through
# the presentation layer: an account with no attempt history renders as never
# successfully observed, never as the placeholder.

CASE_ID="002-status"
CASE_DESCRIPTION="The status command renders each configured account's reading through the presentation layer."

CONFIG_FILE=""

case_preconditions() {
    CONFIG_FILE="$STATE_DIR/aub.toml"
    cat > "$CONFIG_FILE" <<'EOF'
[[accounts]]
name = "work-primary"
provider = "provider-a"
EOF
}

case_steps() {
    step "status" env \
        "HOME=$STATE_DIR/home" \
        "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "$AUB_BIN" -v status
}

case_assertions() {
    assert_exit 0 1
    assert_stderr_contains 1 "run_started"
    # Real rendered content from the presentation layer: the configured account
    # name and the never-observed reading, not the placeholder string.
    assert_stdout_contains 1 "work-primary"
    assert_stdout_contains 1 "no successful sample"
}
