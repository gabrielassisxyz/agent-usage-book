# `aub ingest transcripts` carries each session's working directory from the
# transcript into the ledger, resolves project and repository through the
# alias tables, and `aub rebuild sessions` re-resolves the stored sessions
# after the tables change (aub-4ow0).

CASE_ID="aub-4ow0-working-directory"
CASE_DESCRIPTION="ingest stores the transcript working directory and spend groups by the aliased project; rebuild sessions re-resolves it."

CONFIG=""

case_preconditions() {
    local corpus="$STATE_DIR/transcripts/claude-code"
    mkdir -p "$corpus"

    cat > "$corpus/session.jsonl" <<JSONL
{"type":"assistant","timestamp":"2026-08-25T10:00:00.000Z","sessionId":"s1","cwd":"$STATE_DIR/project","message":{"id":"m1","usage":{"input_tokens":100,"output_tokens":50}}}
{"type":"assistant","timestamp":"2026-08-25T10:05:00.000Z","sessionId":"s1","cwd":"$STATE_DIR/project","message":{"id":"m2","usage":{"input_tokens":30,"output_tokens":12}}}
JSONL

    CONFIG="$STATE_DIR/aub.toml"
    cat > "$CONFIG" <<EOT
[projects]
"$STATE_DIR/project" = "fixture"

[repositories]
"$STATE_DIR/project" = "fixture-repo"

[[transcripts]]
name = "claude-code"
root = "$corpus"
pattern = "**/*.jsonl"
format = "claude-code"
EOT
}

case_steps() {
    step "ingest transcripts" env \
        "HOME=$STATE_DIR/home" \
        "AUB_CONFIG_FILE=$CONFIG" \
        "$AUB_BIN" ingest transcripts
    step "spend by project" env \
        "HOME=$STATE_DIR/home" \
        "AUB_CONFIG_FILE=$CONFIG" \
        "$AUB_BIN" spend --since 2026-08-25 --days 2 --group-by project --refresh never
    step "rebuild sessions" env \
        "HOME=$STATE_DIR/home" \
        "AUB_CONFIG_FILE=$CONFIG" \
        "$AUB_BIN" rebuild sessions
    step "spend by project json" env \
        "HOME=$STATE_DIR/home" \
        "AUB_CONFIG_FILE=$CONFIG" \
        "$AUB_BIN" spend --since 2026-08-25 --days 2 --group-by project \
        --refresh never --format json
}

case_assertions() {
    # The ingest lands the session with its stated directory and reports no
    # mid-session directory change.
    assert_exit 0 1
    assert_stdout_contains 1 "events: written=2 already-ingested=0"
    assert_stdout_contains 1 "working_directory_changes=0"

    # The stored session resolves through the alias table: the project group
    # is the configured logical name, not the unknown bucket.
    assert_exit 0 2
    assert_stdout_contains 2 "project=fixture"
    if grep -qF "project=unknown-project" "$(step_dir 2)/stdout.bin"; then
        echo "step 2 must not show an unknown-project row" >&2
        return 1
    fi

    # Re-resolving the stored sessions rewrites the derived keys in place.
    assert_exit 0 3
    assert_stdout_contains 3 "rebuild sessions: re-resolved 1 sessions"

    assert_exit 0 4
    assert_json_field 4 "grouping[0]" "project"
    assert_json_field 4 "groups[0].key" "project=fixture"
    assert_json_field 4 "groups[0].tokens.input.value" "130"
}
