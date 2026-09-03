# `aub spend` over a seeded transcript corpus: one Claude Code session whose message
# is replayed across three lines with a growing output count, one quarantined
# record, one pi session, one Codex session with a rate-limit-only record, and one
# file last modified before the window. The counts, the replay count, the
# quarantine count and the skip count are all asserted, in both renderings.

CASE_ID="007-spend"
CASE_DESCRIPTION="aub spend refreshes the canonical ledger and reports nested day, session, project and repository token subtotals."

CONFIG_FILE=""

case_preconditions() {
    CONFIG_FILE="$STATE_DIR/aub.toml"
    local corpus="$STATE_DIR/transcripts"
    mkdir -p "$corpus/claude-code/project-a" "$corpus/codex/2026/08/25" "$corpus/pi/project-a"
    cat > "$corpus/claude-code/project-a/session.jsonl" <<'JSONL'
{"type":"assistant","timestamp":"2026-08-25T10:00:00.000Z","sessionId":"s-e2e-1","message":{"id":"msg_e2e_1","usage":{"input_tokens":2,"cache_creation_input_tokens":30000,"cache_read_input_tokens":26000,"output_tokens":100,"service_tier":"standard","server_tool_use":{"web_search_requests":0}}}}
{"type":"assistant","timestamp":"2026-08-25T10:00:01.000Z","sessionId":"s-e2e-1","message":{"id":"msg_e2e_1","usage":{"input_tokens":2,"cache_creation_input_tokens":30000,"cache_read_input_tokens":26000,"output_tokens":400,"service_tier":"standard"}}}
{"type":"assistant","timestamp":"2026-08-25T10:00:02.000Z","sessionId":"s-e2e-1","message":{"id":"msg_e2e_1","usage":{"input_tokens":2,"cache_creation_input_tokens":30000,"cache_read_input_tokens":26000,"output_tokens":913,"service_tier":"standard"}}}
{"type":"assistant","timestamp":"2026-08-25T11:00:00.000Z","sessionId":"s-e2e-1","message":{"id":"msg_e2e_2","usage":{"input_tokens":"not-a-number","output_tokens":5}}}
{"type":"assistant","timestamp":"2026-08-26T00:00:00.000Z","sessionId":"s-e2e-1","message":{"id":"msg_e2e_3","usage":{"input_tokens":10,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":20}}}
JSONL
    cat > "$corpus/claude-code/project-a/old.jsonl" <<'JSONL'
{"type":"assistant","timestamp":"2026-08-25T09:00:00.000Z","sessionId":"s-e2e-0","message":{"id":"msg_e2e_old","usage":{"input_tokens":999,"output_tokens":999}}}
JSONL
    touch -d "2026-08-01T00:00:00Z" "$corpus/claude-code/project-a/old.jsonl"
    cat > "$corpus/codex/2026/08/25/rollout.jsonl" <<'JSONL'
{"timestamp":"2026-08-25T14:31:00.000Z","type":"session_meta","payload":{"type":null,"id":"codex-e2e-1","cwd":"/work/project"}}
{"timestamp":"2026-08-25T14:31:33.849Z","type":"event_msg","payload":{"type":"token_count","info":null,"rate_limits":{}}}
{"timestamp":"2026-08-25T14:32:10.001Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1000,"cached_input_tokens":600,"cache_write_input_tokens":0,"output_tokens":50,"reasoning_output_tokens":5,"total_tokens":1655}}}}
{"timestamp":"2026-08-25T14:33:10.001Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":3000,"cached_input_tokens":1200,"cache_write_input_tokens":0,"output_tokens":150,"reasoning_output_tokens":15,"total_tokens":4365}}}}
JSONL
    cat > "$corpus/pi/project-a/session.jsonl" <<'JSONL'
{"type":"session","version":3,"id":"pi-e2e-1","timestamp":"2026-08-25T23:31:38.741Z","cwd":"/work/project"}
{"type":"message","id":"rec-e2e-1","parentId":null,"timestamp":"2026-08-25T23:33:39.627Z","message":{"role":"assistant","model":"model-a","usage":{"input":700,"output":30,"cacheRead":0,"cacheWrite":0,"reasoning":12,"totalTokens":742,"cost":{"input":0,"output":0,"total":0}}}}
JSONL
    cat > "$CONFIG_FILE" <<EOT
[[transcripts]]
name = "claude-code"
root = "$corpus/claude-code"
pattern = "**/*.jsonl"
format = "claude-code"

[[transcripts]]
name = "codex"
root = "$corpus/codex"
pattern = "**/*.jsonl"
format = "codex"

[[transcripts]]
name = "pi"
root = "$corpus/pi"
pattern = "**/*.jsonl"
format = "pi"
EOT
}

case_steps() {
    step "spend text" env \
        "HOME=$STATE_DIR/home" \
        "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "$AUB_BIN" spend --since 2026-08-25 --days 2 \
        --group-by day --group-by session --group-by project --group-by repository --refresh force
    step "spend json" env \
        "HOME=$STATE_DIR/home" \
        "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "$AUB_BIN" spend --since 2026-08-25 --days 2 \
        --group-by day --group-by session --group-by project --group-by repository \
        --refresh never --format json
    step "spend empty window" env \
        "HOME=$STATE_DIR/home" \
        "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "$AUB_BIN" spend --since 2026-08-27 --refresh never
}

case_assertions() {
    assert_exit 0 1
    # The report reads its canonical event store: the old-mtime transcript is no
    # longer silently skipped, while the replay is still one canonical event.
    assert_stdout_contains 1 "grouped by day, session, project, repository"
    assert_stdout_contains 1 "ingestion generation: 1"
    assert_stdout_contains 1 "day=2026-08-25  input 4701 tokens · output 2092 tokens · cache read 27200 tokens · cache write 30000 tokens"
    assert_stdout_contains 1 "day=2026-08-26  input 10 tokens · output 20 tokens · cache read 0 tokens · cache write 0 tokens"
    assert_stdout_contains 1 "session=claude-code:s-e2e-1"
    assert_stdout_contains 1 "project=unknown-project"
    assert_stdout_contains 1 "repository=unknown-repository"

    assert_exit 0 2
    assert_json_field 2 "command" "spend"
    assert_json_field 2 "schema" "1"
    assert_json_field 2 "window.since" "2026-08-25"
    assert_json_field 2 "ingestion_generation" "1"
    assert_json_field 2 "grouping[0]" "day"
    assert_json_field 2 "grouping[1]" "session"
    assert_json_field 2 "groups[0].tokens.output.value" "2092"
    assert_json_field 2 "groups[0].tokens.output.unit" "tokens"
    assert_json_field 2 "groups[0].coverage" "partial"
    assert_json_field 2 "groups[0].children[0].children[0].children[0].tokens.input.unit" "tokens"
    assert_json_field 2 "ingest.refresh_attempted" "false"
    assert_json_field 2 "ingest.quarantined.wrong_field_type" "1"
    assert_json_field 2 "ingest.files_skipped_before_window" "0"

    # A window with no events is reported as such, never as a bare zero.
    assert_exit 0 3
    assert_stdout_contains 3 "no usage events in the window"
}
