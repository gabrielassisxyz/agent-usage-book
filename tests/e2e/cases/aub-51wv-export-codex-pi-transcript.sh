# aub-51wv: `aub export transcript` renders codex and pi sessions as markdown,
# run against the release binary under the end-to-end runner. The fixture
# ledger carries one codex and one pi session with real transcript files on
# disk: user and assistant text, one tool call with a 2000-character output,
# and one reasoning block each. The codex file also carries a line type the
# renderer has no case for, proving unknown lines are reported, never fatal.

CASE_ID="aub-51wv-export-codex-pi-transcript"
CASE_DESCRIPTION="export transcript renders one codex and one pi session as markdown with whole tool output, thinking blocks, and a skipped-line report."

LEDGER_DB=""
CODEX_TRANSCRIPT=""
PI_TRANSCRIPT=""
EXPORT_CONFIG=""

aub_export() {
    env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR/aub" \
        "AUB_CONFIG_FILE=$EXPORT_CONFIG" \
        "$AUB_BIN" export "$@"
}

case_preconditions() {
    require_command "$AUB_BIN"
    require_command sqlite3
    require_command cat

    LEDGER_DB="$STATE_DIR/aub/ledger.db"
    CODEX_TRANSCRIPT="$STATE_DIR/transcripts/sess-51wv-codex.jsonl"
    PI_TRANSCRIPT="$STATE_DIR/transcripts/sess-51wv-pi.jsonl"
    EXPORT_CONFIG="$STATE_DIR/aub.toml"

    mkdir -p "$(dirname "$CODEX_TRANSCRIPT")"
    cat > "$EXPORT_CONFIG" <<EOT
[export]
clipboard_command = "cat"
EOT
}

case_steps() {
    step "empty-ledger run-keyed export" aub_export --key run-id

    step "write the fixture transcripts" bash -c '
        set -eu
        codex="$1"
        pi="$2"
        big="$(printf "o%.0s" $(seq 1 2000))"
        cat >"$codex" <<JSONL
{"timestamp":"2026-09-20T10:00:00.000Z","type":"session_meta","payload":{"session_id":"51wv-codex-1","id":"51wv-codex-1","timestamp":"2026-09-20T10:00:00.000Z","cwd":"/work/project","originator":"codex_exec","cli_version":"0.147.0","source":"exec","model_provider":"openai"}}
{"timestamp":"2026-09-20T10:01:00.000Z","type":"response_item","payload":{"type":"message","id":"m1","role":"user","content":[{"type":"input_text","text":"Do the thing"}]}}
{"timestamp":"2026-09-20T10:02:00.000Z","type":"response_item","payload":{"type":"message","id":"m2","role":"assistant","content":[{"type":"output_text","text":"On it"}]}}
{"timestamp":"2026-09-20T10:03:00.000Z","type":"response_item","payload":{"type":"function_call","id":"f1","name":"read","arguments":"{\"path\": \"/work/project/README.md\"}","call_id":"call_e2e1"}}
{"timestamp":"2026-09-20T10:04:00.000Z","type":"response_item","payload":{"type":"function_call_output","id":"o1","call_id":"call_e2e1","output":[{"type":"input_text","text":"$big"}]}}
{"timestamp":"2026-09-20T10:05:00.000Z","type":"response_item","payload":{"type":"reasoning","id":"r1","summary":[],"encrypted_content":"sanitized"}}
{"timestamp":"2026-09-20T10:06:00.000Z","type":"event_msg","payload":{"type":"token_count","info":null}}
{"type":"future_envelope","payload":{}}
JSONL
        cat >"$pi" <<JSONL
{"type":"session","version":3,"id":"51wv-pi-1","timestamp":"2026-09-20T10:00:00.000Z","cwd":"/work/project"}
{"type":"message","id":"u1","message":{"role":"user","content":[{"type":"text","text":"Do the thing"}]}}
{"type":"message","id":"a1","message":{"role":"assistant","content":[{"type":"thinking","thinking":"Plan first"},{"type":"toolCall","id":"call_e2e2","name":"read","arguments":{"path":"/work/project/README.md"}}]}}
{"type":"message","id":"t1","message":{"role":"toolResult","toolCallId":"call_e2e2","toolName":"read","content":[{"type":"text","text":"$big"}]}}
{"type":"message","id":"a2","message":{"role":"assistant","content":[{"type":"text","text":"Finished"}]}}
JSONL
    ' _ "$CODEX_TRANSCRIPT" "$PI_TRANSCRIPT"

    step "seed the ledger fixture" sqlite3 "$LEDGER_DB" "
        INSERT INTO session (source, native_session_id, run_id, project_key, repository_key, start, end)
        VALUES ('codex','51wv-codex-1',NULL,'proj-codex','repo-e2e',100,150),
               ('pi','51wv-pi-1',NULL,'proj-pi','repo-e2e',200,250);
        INSERT INTO usage_event (canonical_event_id, session_id, evidence_kind, source_provenance, parser_version, created_at)
        VALUES ('ce-51wv-codex','51wv-codex-1','transcript','$CODEX_TRANSCRIPT','codex-2',100),
               ('ce-51wv-pi','51wv-pi-1','transcript','$PI_TRANSCRIPT','pi-2',200);
        INSERT INTO usage_component (event_id, token_class, count)
        VALUES (1,'input',10),(2,'input',10);
        INSERT INTO usage_occurrence (source_namespace, native_event_id, parser_version, source_file, occurred_at, event_id)
        VALUES ('codex','ne-51wv-codex','codex-2','$CODEX_TRANSCRIPT',100,1),
               ('pi','ne-51wv-pi','pi-2','$PI_TRANSCRIPT',200,2);
    "

    step "render the codex session to stdout" aub_export transcript 51wv-codex-1 -p
    step "render the codex session with tools" aub_export transcript 51wv-codex-1 -p --include-tools
    step "render the pi session with thinking" aub_export transcript 51wv-pi-1 -p --include-thinking
    step "render the pi session with tools" aub_export transcript 51wv-pi-1 -p --include-tools
}

case_assertions() {
    assert_exit 0 1
    assert_stdout_contains 1 '"schema":1'

    assert_exit 0 2

    assert_exit 0 3

    # The codex session renders user and assistant text with no tool or
    # thinking traffic without flags.
    assert_exit 0 4
    assert_stdout_contains 4 "# codex · proj-codex · 51wv-codex-1"
    assert_stdout_contains 4 "## User"
    assert_stdout_contains 4 "Do the thing"
    assert_stdout_contains 4 "## Assistant"
    if grep -qF "**Tool:" "$(step_dir 4)/stdout.bin"; then
        record_assertion "plain codex rendering hides tool traffic" "absent" "present" "fail"
        CASE_FAILED=1
    else
        record_assertion "plain codex rendering hides tool traffic" "absent" "absent" "pass"
    fi

    # With tools the 2000-character output arrives whole, paired to its
    # call, and the unknown line is reported on stderr without failing.
    assert_exit 0 5
    assert_stdout_contains 5 "**Tool: read**"
    assert_stdout_contains 5 "/work/project/README.md"
    big="$(printf 'o%.0s' $(seq 1 2000))"
    if grep -qF "$big" "$(step_dir 5)/stdout.bin"; then
        record_assertion "codex tool output arrives whole" "whole" "whole" "pass"
    else
        record_assertion "codex tool output arrives whole" "whole" "truncated" "fail"
        CASE_FAILED=1
    fi
    assert_stderr_contains 5 "skipped: 1 lines of type future_envelope"

    # The pi thinking block renders as a blockquote under its flag.
    assert_exit 0 6
    assert_stdout_contains 6 "# pi · proj-pi · 51wv-pi-1"
    assert_stdout_contains 6 "> Plan first"

    # With tools the pi result arrives whole, paired to its call.
    assert_exit 0 7
    assert_stdout_contains 7 "**Tool: read**"
    if grep -qF "$big" "$(step_dir 7)/stdout.bin"; then
        record_assertion "pi tool output arrives whole" "whole" "whole" "pass"
    else
        record_assertion "pi tool output arrives whole" "whole" "truncated" "fail"
        CASE_FAILED=1
    fi
}
