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
    cat > "$corpus/claude-code/project-a/seven-day.jsonl" <<'JSONL'
{"type":"assistant","timestamp":"2026-08-27T10:00:00.000Z","sessionId":"s-e2e-day-27","message":{"id":"msg_e2e_day_27","usage":{"input_tokens":10,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":1}}}
{"type":"assistant","timestamp":"2026-08-28T10:00:00.000Z","sessionId":"s-e2e-day-28","message":{"id":"msg_e2e_day_28","usage":{"input_tokens":10,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":1}}}
{"type":"assistant","timestamp":"2026-08-29T10:00:00.000Z","sessionId":"s-e2e-day-29","message":{"id":"msg_e2e_day_29","usage":{"input_tokens":10,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":1}}}
{"type":"assistant","timestamp":"2026-08-30T10:00:00.000Z","sessionId":"s-e2e-day-30","message":{"id":"msg_e2e_day_30","usage":{"input_tokens":10,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":1}}}
{"type":"assistant","timestamp":"2026-08-31T10:00:00.000Z","sessionId":"s-e2e-day-31","message":{"id":"msg_e2e_day_31","usage":{"input_tokens":10,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":1}}}
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
    # Today's UTC date, fixed once so the seed and the assertions agree even if
    # the run crosses midnight: the backward `--days 2` window reads relative
    # to the run, so the corpus for that step must carry dates the resolver
    # actually produces (aub-satk).
    TODAY="$(date -u +%F)"
    YESTERDAY="$(date -u -d 'yesterday' +%F)"
    cat > "$corpus/claude-code/project-a/today.jsonl" <<JSONL
{"type":"assistant","timestamp":"${TODAY}T08:00:00.000Z","sessionId":"s-e2e-today","message":{"id":"msg_e2e_today","usage":{"input_tokens":11,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":4,"service_tier":"standard"}}}
JSONL
    cat > "$corpus/codex/2026/08/25/today-rollout.jsonl" <<JSONL
{"timestamp":"${TODAY}T09:00:00.000Z","type":"session_meta","payload":{"type":null,"id":"codex-e2e-today","cwd":"/work/project"}}
{"timestamp":"${TODAY}T09:01:00.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":220,"cached_input_tokens":0,"cache_write_input_tokens":0,"output_tokens":11,"reasoning_output_tokens":0,"total_tokens":231}}}}
JSONL
    cat > "$corpus/pi/project-a/today.jsonl" <<JSONL
{"type":"session","version":3,"id":"pi-e2e-today","timestamp":"${TODAY}T10:00:00.000Z","cwd":"/work/project"}
{"type":"message","id":"rec-e2e-today","parentId":null,"timestamp":"${TODAY}T10:01:00.000Z","message":{"role":"assistant","model":"model-a","usage":{"input":33,"output":3,"cacheRead":0,"cacheWrite":0,"reasoning":0,"totalTokens":36,"cost":{"input":0,"output":0,"total":0}}}}
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
    step "spend seven-day boxed text" env \
        "HOME=$STATE_DIR/home" \
        "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "$AUB_BIN" spend --since 2026-08-25 --days 7 --group-by day --refresh never
    step "spend empty window" env \
        "HOME=$STATE_DIR/home" \
        "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "$AUB_BIN" spend --since 2026-09-02 --until 2026-09-03 --refresh never
    step "spend backward window and harness filter" env \
        "HOME=$STATE_DIR/home" \
        "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "$AUB_BIN" spend --days 2 --group-by harness --harness codex --refresh never
    step "spend backward window json" env \
        "HOME=$STATE_DIR/home" \
        "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "$AUB_BIN" spend --days 2 --group-by harness --harness codex \
        --refresh never --format json
}

case_assertions() {
    assert_exit 0 1
    # The report reads its canonical event store: the old-mtime transcript is no
    # longer silently skipped, while the replay is still one canonical event.
    assert_stdout_contains 1 "┌─ spend · 2026-08-25"
    # Four dimensions: the day titles each section and every row names its
    # session and project, which the innermost-only label used to drop.
    assert_stdout_contains 1 "· by day, session, project"
    assert_stdout_contains 1 "│  day · 2026-08-25"
    assert_stdout_matches 1 "^│  session · project · repository +input "
    assert_stdout_matches 1 "^│  claude-code:s-e2e-1 +· unknown +· unknown +2 +913 +26\\.0k "
    assert_stdout_matches 1 "^│  total +4711 +2112 +27\\.2k "
    assert_stdout_contains 1 "generation 1"
    assert_stdout_contains 1 "└"

    assert_exit 0 2
    assert_json_field 2 "command" "spend"
    assert_json_field 2 "schema" "5"
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

    # The seven-day pipe exercises the real box from its first rail to its last,
    # with one visible row for every UTC day and a grand total.
    assert_exit 0 3
    assert_stdout_contains 3 "┌─ spend · 2026-08-25 → 2026-08-31 · 7 days UTC · by day"
    assert_stdout_matches 3 "^│  2026-08-25 +4701 +2092 +27\\.2k +30\\.0k "
    assert_stdout_matches 3 "^│  2026-08-26 +10 +20 "
    assert_stdout_matches 3 "^│  2026-08-27 +10 +1 "
    assert_stdout_matches 3 "^│  2026-08-28 +10 +1 "
    assert_stdout_matches 3 "^│  2026-08-29 +10 +1 "
    assert_stdout_matches 3 "^│  2026-08-30 +10 +1 "
    assert_stdout_matches 3 "^│  2026-08-31 +10 +1 "
    assert_stdout_matches 3 "^│  total +4761 +2117 +27\\.2k +30\\.0k "
    if [ "$(grep -cE '^│  2026-08-[0-9]{2} ' "$(step_dir 3)/stdout.bin")" != 7 ]; then
        echo "step 3 must render exactly seven day rows" >&2
        return 1
    fi
    if ! tail -n 1 "$(step_dir 3)/stdout.bin" | grep -qE '^└─+┘$'; then
        echo "step 3 must end on the bottom rail of the box" >&2
        return 1
    fi

    # A window with no events is reported as such, never as a bare zero.
    assert_exit 0 4
    assert_stdout_contains 4 "no usage events in the window"

    # The backward `--days 2` window is the two days ending today, today
    # included, and the harness filter keeps only the codex rows while the
    # footer names what the filter excluded (aub-satk).
    assert_exit 0 5
    assert_stdout_contains 5 "· by harness"
    assert_stdout_matches 5 "^│  codex +220 +11 "
    assert_stdout_contains 5 "excluded: 2 sessions, of which 0 unknown-harness"
    # Rows carry the bare dimension value, so the guard matches the row cell,
    # not the harness=... key the pre-box text printed.
    if grep -qE "^│  claude-code |harness=claude-code" "$(step_dir 5)/stdout.bin"; then
        echo "step 5 must not show a filtered-out harness row" >&2
        return 1
    fi
    if grep -qE "^│  pi |harness=pi" "$(step_dir 5)/stdout.bin"; then
        echo "step 5 must not show a filtered-out harness row" >&2
        return 1
    fi

    assert_exit 0 6
    assert_json_field 6 "window.since" "$YESTERDAY"
    assert_json_field 6 "grouping[0]" "harness"
    assert_json_field 6 "groups[0].key" "harness=codex"
    assert_json_field 6 "groups[0].tokens.input.value" "220"
    assert_json_field 6 "filters[0].flag" "--harness"
    assert_json_field 6 "filters[0].excluded.sessions" "2"
    assert_json_field 6 "filters[0].excluded.events" "2"
    assert_json_field 6 "filters[0].excluded.unknown_events" "0"
}
