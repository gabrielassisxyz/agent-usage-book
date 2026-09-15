# aub-xpfl: `aub export transcript` renders one session's transcript as
# markdown from the ledger's own session-to-file map, run against the release
# binary under the end-to-end runner. The fixture ledger carries the bead's
# resolution cases (`aaaa0001` claude-code, `aaaa0002` codex, the `01a0318b`
# codex pair) with real transcript files on disk for the claude-code session:
# a parent file and one `subagents/agent-x.jsonl` sharing its session id.

CASE_ID="aub-xpfl-export-transcript"
CASE_DESCRIPTION="export transcript resolves an id prefix, renders the session as markdown, and delivers it to stdout, file or clipboard command."

LEDGER_DB=""
TRANSCRIPT_PARENT=""
TRANSCRIPT_SUBAGENT=""
TRANSCRIPT_MISSING=""
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
    TRANSCRIPT_PARENT="$STATE_DIR/transcripts/sess-aaaa0001.jsonl"
    TRANSCRIPT_SUBAGENT="$STATE_DIR/transcripts/sess-aaaa0001/subagents/agent-x.jsonl"
    TRANSCRIPT_MISSING="$STATE_DIR/transcripts/gone.jsonl"
    EXPORT_CONFIG="$STATE_DIR/aub.toml"

    mkdir -p "$(dirname "$TRANSCRIPT_PARENT")" "$(dirname "$TRANSCRIPT_SUBAGENT")"
    cat > "$EXPORT_CONFIG" <<EOT
[export]
clipboard_command = "cat"
EOT
}

case_steps() {
    # The first export runs against an empty ledger through the untouched
    # `--key` path, creating and migrating the database through the real
    # command path.
    step "empty-ledger run-keyed export" aub_export --key run-id

    step "write the fixture transcripts" bash -c '
        set -eu
        parent="$1"
        subagent="$2"
        big="$(printf "x%.0s" $(seq 1 3000))"
        cat >"$parent" <<JSONL
{"type":"user","message":{"role":"user","content":"Do the thing"},"timestamp":"2026-09-06T10:00:00.000Z","sessionId":"aaaa0001-1111-4222-8333-444444444444"}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"On it"},{"type":"tool_use","id":"toolu_1","name":"Read","input":{"data":"$big"}}]},"timestamp":"2026-09-06T10:01:00.000Z","sessionId":"aaaa0001-1111-4222-8333-444444444444"}
{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"README contents"}]},"timestamp":"2026-09-06T10:02:00.000Z","sessionId":"aaaa0001-1111-4222-8333-444444444444"}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"First do this","signature":"sig"},{"type":"text","text":"Finished"}]},"timestamp":"2026-09-06T10:03:00.000Z","sessionId":"aaaa0001-1111-4222-8333-444444444444"}
JSONL
        cat >"$subagent" <<JSONL
{"type":"assistant","message":{"role":"assistant","content":"subagent reply"},"timestamp":"2026-09-06T10:04:00.000Z","sessionId":"aaaa0001-1111-4222-8333-444444444444"}
JSONL
    ' _ "$TRANSCRIPT_PARENT" "$TRANSCRIPT_SUBAGENT"

    # The bead fixture: the claude-code session starts later, so `aaaa
    # --latest` renders it, while the `01a0318b` pair is codex-only and its
    # latest has no renderer yet.
    step "seed the ledger fixture" sqlite3 "$LEDGER_DB" "
        INSERT INTO session (source, native_session_id, run_id, project_key, repository_key, start, end)
        VALUES ('claude-code','aaaa0001-1111-4222-8333-444444444444',NULL,'proj-alpha','repo-alpha',200,300),
               ('codex','aaaa0002-1111-7222-9333-444444444444',NULL,'proj-beta','repo-beta',100,150),
               ('codex','01a0318b-1aaa',NULL,'proj-gamma','repo-gamma',300,350),
               ('codex','01a0318b-2bbb',NULL,'proj-gamma','repo-gamma',400,450),
               ('claude-code','deadbeef-0000-0000-0000-000000000000',NULL,'proj-alpha','repo-alpha',50,60);
        INSERT INTO usage_event (canonical_event_id, session_id, evidence_kind, source_provenance, parser_version, created_at)
        VALUES ('ce-xpfl-1','aaaa0001-1111-4222-8333-444444444444','transcript','$TRANSCRIPT_PARENT','claude-code-1',200),
               ('ce-xpfl-2','aaaa0001-1111-4222-8333-444444444444','transcript','$TRANSCRIPT_SUBAGENT','claude-code-1',210),
               ('ce-xpfl-3','deadbeef-0000-0000-0000-000000000000','transcript','$TRANSCRIPT_MISSING','claude-code-1',50);
        INSERT INTO usage_component (event_id, token_class, count)
        VALUES (1,'input',10),(2,'input',10),(3,'input',10);
        INSERT INTO usage_occurrence (source_namespace, native_event_id, parser_version, source_file, occurred_at, event_id)
        VALUES ('claude-code','ne-xpfl-1','claude-code-1','$TRANSCRIPT_PARENT',200,1),
               ('claude-code','ne-xpfl-2','claude-code-1','$TRANSCRIPT_SUBAGENT',210,2),
               ('claude-code','ne-xpfl-3','claude-code-1','$TRANSCRIPT_MISSING',50,3);
    "

    step "render to stdout" aub_export transcript aaaa0001 -p
    step "ambiguous prefix fails" aub_export transcript aaaa -p
    step "latest resolves the ambiguity" aub_export transcript aaaa --latest -p
    step "codex has no renderer yet" aub_export transcript 01a0318b --harness codex --latest -p
    step "unknown id names itself" aub_export transcript zzzz -p
    step "clipboard command receives the markdown" aub_export transcript aaaa0001 -c
    step "default file output" aub_export transcript aaaa0001 -o
    step "missing file renders the rest" aub_export transcript deadbeef -p
}

case_assertions() {
    assert_exit 0 1
    assert_stdout_contains 1 '"schema":1'

    assert_exit 0 2

    assert_exit 0 3

    # The first line is the heading for the claude-code session, and the
    # parent renders before the subagent section.
    assert_exit 0 4
    assert_stdout_contains 4 "# claude-code · proj-alpha · aaaa0001-1111-4222-8333-444444444444"
    assert_stdout_contains 4 "## User"
    assert_stdout_contains 4 "Do the thing"
    assert_stdout_contains 4 "## Subagent agent-x.jsonl"
    assert_stdout_contains 4 "subagent reply"
    if grep -qF "**Tool:" "$(step_dir 4)/stdout.bin"; then
        record_assertion "plain rendering hides tool traffic" "absent" "present" "fail"
        CASE_FAILED=1
    else
        record_assertion "plain rendering hides tool traffic" "absent" "absent" "pass"
    fi

    # The planted negative for the listing: a prefix that matches one
    # session must render, so the failure below proves the count, not the
    # exit code alone.
    assert_exit 2 5
    assert_stderr_contains 5 "matches 2 sessions"
    assert_stderr_contains 5 "--latest"
    assert_stderr_contains 5 "aaaa0001-1111-4222-8333-444444444444"
    assert_stderr_contains 5 "aaaa0002-1111-7222-9333-444444444444"

    assert_exit 0 6
    assert_stdout_contains 6 "# claude-code · proj-alpha · aaaa0001-1111-4222-8333-444444444444"

    assert_exit 2 7
    assert_stderr_contains 7 "no transcript renderer for harness 'codex'"

    assert_exit 2 8
    assert_stderr_contains 8 "no session matches 'zzzz'"

    # `-c` with `cat` as the clipboard command: the markdown arrives on the
    # child's stdin and back on our stdout.
    assert_exit 0 9
    assert_stdout_contains 9 "# claude-code · proj-alpha · aaaa0001-1111-4222-8333-444444444444"

    # `-o` alone creates `~/agent-transcripts/` under the test's HOME and
    # prints the path it wrote.
    assert_exit 0 10
    out_path="$(cat "$(step_dir 10)/stdout.bin")"
    case "$out_path" in
        "$STATE_DIR/home/agent-transcripts/"*.md) record_assertion "default file lands under ~/agent-transcripts" "$out_path" "under-home" "pass" ;;
        *) record_assertion "default file lands under ~/agent-transcripts" "$out_path" "under-home" "fail"; CASE_FAILED=1 ;;
    esac
    if [ -f "$out_path" ] && grep -qF "# claude-code" "$out_path"; then
        record_assertion "default file holds the heading" "heading" "heading" "pass"
    else
        record_assertion "default file holds the heading" "heading" "missing" "fail"
        CASE_FAILED=1
    fi

    # The missing file is reported by path, the files that exist still
    # render, and the exit is non-zero.
    assert_exit 8 11
    assert_stderr_contains 11 "missing: $TRANSCRIPT_MISSING"
}
