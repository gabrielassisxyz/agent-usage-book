# aub-m76e: `aub export transcript` renders opencode sessions from opencode.db
# as markdown, run against the release binary under the end-to-end runner. The
# fixture database carries one session with a user turn, an assistant turn
# with a finished tool part, and an assistant turn with a reasoning part; the
# ledger maps the session to the database path the way ingest records it. A
# second ledger session points at the same database without existing in it,
# proving a pruned database fails by id instead of rendering an empty
# document.

CASE_ID="aub-m76e-export-opencode-transcript"
CASE_DESCRIPTION="export transcript renders one opencode session as markdown with whole tool output and thinking blocks, and reports a pruned session by id."

LEDGER_DB=""
OPENCODE_DB=""
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
    OPENCODE_DB="$STATE_DIR/opencode.db"
    EXPORT_CONFIG="$STATE_DIR/aub.toml"

    mkdir -p "$(dirname "$OPENCODE_DB")"
    cat > "$EXPORT_CONFIG" <<EOT
[export]
clipboard_command = "cat"
EOT
}

case_steps() {
    step "empty-ledger run-keyed export" aub_export --key run-id

    step "write the fixture opencode database" sqlite3 "$OPENCODE_DB" "CREATE TABLE session (id TEXT PRIMARY KEY); CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL); CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT NOT NULL, session_id TEXT NOT NULL, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL); INSERT INTO session (id) VALUES ('m76e-opencode-1'); INSERT INTO message (id, session_id, time_created, time_updated, data) VALUES ('m76e-u1', 'm76e-opencode-1', 100, 100, '{\"role\":\"user\",\"time\":{\"created\":100}}'), ('m76e-a1', 'm76e-opencode-1', 200, 300, '{\"role\":\"assistant\",\"time\":{\"created\":200,\"completed\":300}}'), ('m76e-a2', 'm76e-opencode-1', 400, 500, '{\"role\":\"assistant\",\"time\":{\"created\":400,\"completed\":500}}'); INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) VALUES ('m76e-p1', 'm76e-u1', 'm76e-opencode-1', 100, 100, '{\"type\":\"text\",\"text\":\"Do the thing\"}'), ('m76e-p2', 'm76e-a1', 'm76e-opencode-1', 200, 200, '{\"snapshot\":\"abc\",\"type\":\"step-start\"}'), ('m76e-p3', 'm76e-a1', 'm76e-opencode-1', 210, 210, '{\"type\":\"text\",\"text\":\"On it\"}'), ('m76e-p4', 'm76e-a1', 'm76e-opencode-1', 220, 290, '{\"type\":\"tool\",\"tool\":\"read\",\"callID\":\"call_e2e1\",\"state\":{\"status\":\"completed\",\"input\":{\"path\":\"/work/project/README.md\"},\"output\":\"line one\nline two\nE2E_TAIL_MARKER_7f3a\"}}'), ('m76e-p5', 'm76e-a1', 'm76e-opencode-1', 300, 300, '{\"reason\":\"tool-calls\",\"type\":\"step-finish\"}'), ('m76e-p6', 'm76e-a2', 'm76e-opencode-1', 400, 400, '{\"type\":\"reasoning\",\"text\":\"Plan first, then read.\"}'), ('m76e-p7', 'm76e-a2', 'm76e-opencode-1', 410, 410, '{\"type\":\"text\",\"text\":\"Finished\"}');"



    step "seed the ledger fixture" sqlite3 "$LEDGER_DB" "
        INSERT INTO session (source, native_session_id, run_id, project_key, repository_key, start, end)
        VALUES ('opencode','m76e-opencode-1',NULL,'proj-opencode','repo-e2e',100,150),
               ('opencode','m76e-pruned-1',NULL,'proj-opencode','repo-e2e',200,250);
        INSERT INTO usage_event (canonical_event_id, session_id, evidence_kind, source_provenance, parser_version, created_at)
        VALUES ('ce-m76e-opencode','m76e-opencode-1','transcript','$OPENCODE_DB','opencode-3',100),
               ('ce-m76e-pruned','m76e-pruned-1','transcript','$OPENCODE_DB','opencode-3',200);
        INSERT INTO usage_component (event_id, token_class, count)
        VALUES (1,'input',10),(2,'input',10);
        INSERT INTO usage_occurrence (source_namespace, native_event_id, parser_version, source_file, occurred_at, event_id)
        VALUES ('opencode','ne-m76e-opencode','opencode-3','$OPENCODE_DB',100,1),
               ('opencode','ne-m76e-pruned','opencode-3','$OPENCODE_DB',200,2);
    "

    step "render the opencode session to stdout" aub_export transcript m76e-opencode-1 -p
    step "render the opencode session with tools" aub_export transcript m76e-opencode-1 -p --include-tools
    step "render the opencode session with thinking" aub_export transcript m76e-opencode-1 -p --include-thinking
    step "render the pruned session" aub_export transcript m76e-pruned-1 -p
}

case_assertions() {
    assert_exit 0 1
    assert_stdout_contains 1 '"schema":1'

    # The plain rendering shows user and assistant text with no tool or
    # thinking traffic without flags.
    assert_exit 0 4
    assert_stdout_contains 4 "# opencode · proj-opencode · m76e-opencode-1"
    assert_stdout_contains 4 "## User"
    assert_stdout_contains 4 "Do the thing"
    assert_stdout_contains 4 "## Assistant"
    assert_stdout_contains 4 "On it"
    assert_stdout_contains 4 "Finished"
    if grep -qF "**Tool:" "$(step_dir 4)/stdout.bin"; then
        record_assertion "plain opencode rendering hides tool traffic" "absent" "present" "fail"
        CASE_FAILED=1
    else
        record_assertion "plain opencode rendering hides tool traffic" "absent" "absent" "pass"
    fi

    # With tools the whole output arrives, paired to its call.
    assert_exit 0 5
    assert_stdout_contains 5 "**Tool: read**"
    assert_stdout_contains 5 "/work/project/README.md"
    assert_stdout_contains 5 "line one"
    assert_stdout_contains 5 "E2E_TAIL_MARKER_7f3a"

    # The reasoning block renders as a blockquote under its flag.
    assert_exit 0 6
    assert_stdout_contains 6 "> Plan first, then read."

    # A session the database no longer holds fails by id, never with an
    # empty document.
    assert_exit 8 7
    assert_stderr_contains 7 "session m76e-pruned-1 not found in"
}
