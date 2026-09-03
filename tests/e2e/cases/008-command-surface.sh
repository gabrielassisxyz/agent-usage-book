# The binary explains itself: help states each shipping command's question and
# refusal boundary, accepted report formats emit JSON, and rejected flags say why.

CASE_ID="008-command-surface"
CASE_DESCRIPTION="help, version and usage errors are visible, not silent exits."

CONFIG_FILE=""

case_preconditions() {
    # spend refuses --format json with no configured source: an empty, existing
    # root is enough to prove the flag itself works, without duplicating
    # 007-spend's corpus and assertions.
    CONFIG_FILE="$STATE_DIR/aub.toml"
    mkdir -p "$STATE_DIR/transcripts/claude-code"
    cat > "$CONFIG_FILE" <<EOT
[[transcripts]]
name = "claude-code"
root = "$STATE_DIR/transcripts/claude-code"
pattern = "**/*.jsonl"
format = "claude-code"
EOT
}

case_steps() {
    step "help" "$AUB_BIN" --help
    step "version" "$AUB_BIN" --version
    step "unknown argument" "$AUB_BIN" --definitely-not-a-flag
    step "format refused" "$AUB_BIN" config --format json
    step "explain refused" "$AUB_BIN" config --explain
    step "status json" env "HOME=$STATE_DIR/home" "$AUB_BIN" status --format json
    step "spend json" env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG_FILE" "$AUB_BIN" spend --format json
}

case_assertions() {
    assert_exit 0 1
    assert_stdout_contains 1 "status"
    assert_stdout_contains 1 "spend"
    assert_stdout_contains 1 "config"
    assert_golden 1 "$REPO_ROOT/tests/e2e/help.txt"
    assert_exit 0 2
    assert_stdout_contains 2 "aub "
    assert_exit 2 3
    assert_stderr_contains 3 "aub: unknown argument: --definitely-not-a-flag"
    assert_exit 2 4
    assert_stderr_contains 4 "config prints provenance, not a report"
    assert_exit 2 5
    assert_stderr_contains 5 "config does not accept --explain: config derives no quantity"
    assert_stderr_contains 5 "next: run aub --help"
    assert_exit 0 6
    assert_json_field 6 command status
    assert_exit 0 7
    assert_json_field 7 command spend
}
