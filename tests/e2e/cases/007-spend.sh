# `aub spend` over a seeded transcript corpus: one Claude Code session whose message
# is replayed across three lines with a growing output count, one quarantined
# record, one pi session, one Codex session with a rate-limit-only record, and one
# file last modified before the window. The counts, the replay count, the
# quarantine count and the skip count are all asserted, in both renderings.

CASE_ID="007-spend"
CASE_DESCRIPTION="aub spend reports per-day, per-source token usage with its ingest summary over a seeded corpus."

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
        "$AUB_BIN" spend --since 2026-08-25 --days 2
    step "spend json" env \
        "HOME=$STATE_DIR/home" \
        "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "$AUB_BIN" spend --since 2026-08-25 --days 2 --format json
    step "spend empty window" env \
        "HOME=$STATE_DIR/home" \
        "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "$AUB_BIN" spend --since 2026-08-27
}

case_assertions() {
    assert_exit 0 1
    # The replayed message counts once, at its final output, and the two earlier
    # snapshots are replays; the quarantined record is counted, not dropped.
    assert_stdout_contains 1 "2026-08-25 claude-code  input 2 tokens · output 913 tokens · cache read 26000 tokens · cache write 30000 tokens (complete)"
    assert_stdout_contains 1 "2026-08-26 claude-code  input 10 tokens · output 20 tokens · cache read 0 tokens · cache write 0 tokens (complete)"
    # Codex: the last cumulative record, with its reasoning class preserved as an unknown component.
    assert_stdout_contains 1 "2026-08-25 codex  input 3000 tokens · output 150 tokens · cache read 1200 tokens · cache write 0 tokens · reasoning_output_tokens 15 tokens (complete)"
    assert_stdout_contains 1 "2026-08-25 pi  input 700 tokens · output 30 tokens · cache read 0 tokens · cache write 0 tokens · reasoning 12 tokens (complete)"
    assert_stdout_contains 1 "ingest: 3 files read, 1 skipped (unchanged before the window), 0 unreadable · 4 canonical events in window, 0 outside, 0 undated · 2 replayed occurrences, 0 collisions, 1 without identity · 1 quarantined (wrong_field_type 1)"

    assert_exit 0 2
    assert_json_field 2 "command" "spend"
    assert_json_field 2 "schema" "1"
    assert_json_field 2 "window.since" "2026-08-25"
    assert_json_field 2 "groups[0].tokens.output.value" "913"
    assert_json_field 2 "groups[0].tokens.output.unit" "tokens"
    assert_json_field 2 "groups[0].coverage" "complete"
    assert_json_field 2 "ingest.replayed_occurrences" "2"
    assert_json_field 2 "ingest.quarantined.wrong_field_type" "1"
    assert_json_field 2 "ingest.files_skipped_before_window" "1"

    # A window with no events is reported as such, never as a bare zero.
    assert_exit 0 3
    assert_stdout_contains 3 "no usage events in the window"
}
