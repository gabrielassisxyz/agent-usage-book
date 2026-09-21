# `aub backup --scheduled` stands down cleanly (aub-7xmr): with
# `backup.scheduled = false` it names the key and exits zero without writing,
# and with no `backup.destination` configured it says backups are not
# configured and exits zero. Both run against the release binary because the
# property under test is the process exit code and its stdout line, not one
# function call. The destination directory is pre-created empty so the third
# step proves nothing was written by listing it, rather than by asserting
# about a path that was never there.

CASE_ID="aub-7xmr-scheduled-backup"
CASE_DESCRIPTION="aub backup --scheduled exits zero without writing when disabled by the key or when no destination is configured."

aub_scheduled() {
    env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR/aub" \
        "AUB_CONFIG_FILE=$1" \
        "AUB_LOG_LEVEL=off" \
        "$AUB_BIN" backup --scheduled
}

case_preconditions() {
    require_command "$AUB_BIN"
    mkdir -p "$STATE_DIR/home" "$STATE_DIR/backups"
    cat > "$STATE_DIR/disabled.toml" <<CFG_EOF
[backup]
destination = "$STATE_DIR/backups"
scheduled = false
CFG_EOF
    # No [backup] section at all: every key resolves to its default, which
    # means scheduled without a destination.
    : > "$STATE_DIR/nodeest.toml"
}

case_steps() {
    step "scheduled run disabled by the key" \
        aub_scheduled "$STATE_DIR/disabled.toml"
    step "scheduled run with no destination" \
        aub_scheduled "$STATE_DIR/nodeest.toml"
    step "list the destination contents" \
        find "$STATE_DIR/backups" -mindepth 1
}

case_assertions() {
    assert_exit 0 1
    assert_stdout_contains 1 "backup.scheduled"

    assert_exit 0 2
    assert_stdout_contains 2 "backups are not configured"

    # The disabled run must not have written anything: the pre-created
    # destination lists nothing.
    assert_exit 0 3
    if [ -z "$(cat "$(step_dir 3)/stdout.txt")" ]; then
        record_assertion "destination stays empty" "empty" "empty" "pass"
    else
        record_assertion "destination stays empty" "empty" "$(cat "$(step_dir 3)/stdout.txt")" "fail"
        CASE_FAILED=1
    fi
}
