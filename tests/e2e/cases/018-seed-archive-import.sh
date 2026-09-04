# `aub import seed-archive` crosses the administrative boundary deliberately:
# it requires an independently verified archive before it writes, emits only a
# content digest for the source, and keeps an exact rerun from manufacturing a
# second historical timeline.

CASE_ID="018-seed-archive-import"
CASE_DESCRIPTION="Seed archive imports only after backup verification, remains idempotent, quarantines malformed rows, and never prints the source path."

SOURCE=""
MALFORMED_SOURCE=""
ARCHIVE=""

aub_seed() {
    env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR/aub" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "$AUB_BIN" "$@"
}

case_preconditions() {
    require_command "$AUB_BIN"
    require_command sqlite3

    SOURCE="$STATE_DIR/seed-archive.jsonl"
    MALFORMED_SOURCE="$STATE_DIR/malformed-seed.jsonl"
    ARCHIVE="$STATE_DIR/verified-archive"
    cat > "$STATE_DIR/aub.toml" <<EOF
state.dir = "$STATE_DIR/aub"

[[accounts]]
name = "primary"
provider = "anthropic"
EOF
    cat > "$SOURCE" <<'JSONL'
{"received_at":"2026-08-26T03:00:00Z","account":"primary","tool":"aub-meter","tool_version":"0.1.0","plan":"pro","reading":{"generatedAt":"2026-08-26T02:59:58Z","providers":[{"provider":"anthropic","windows":[{"id":"five_hour","percentUsed":10,"resetsAt":"2026-08-26T05:00:00Z","windowSeconds":18000},{"id":"seven_day","percentUsed":20,"resetsAt":"2026-09-02T00:00:00Z","windowSeconds":604800}]}]}}
{"received_at":"2026-08-26T03:06:00Z","account":"primary","tool":"aub-meter","tool_version":"0.1.0","failure":"spawn_failed","exit_code":1}
not-json
JSONL
    printf '%s\n' 'not-json' > "$MALFORMED_SOURCE"
}

case_steps() {
    step "initialize the ledger" aub_seed export --key run-id
    step "create a verified backup" aub_seed backup "$ARCHIVE"
    step "import the seed archive source" aub_seed import seed-archive --source "$SOURCE" --backup "$ARCHIVE" -v
    step "repeat the same import" aub_seed import seed-archive --source "$SOURCE" --backup "$ARCHIVE"
    step "quarantine a malformed source" aub_seed import seed-archive --source "$MALFORMED_SOURCE" --backup "$ARCHIVE"
    step "refuse an unverified backup" aub_seed import seed-archive --source "$SOURCE" --backup "$STATE_DIR/not-an-archive"
    step "read durable import cardinalities" sqlite3 "$STATE_DIR/aub/ledger.db" "SELECT (SELECT COUNT(*) FROM meter_attempt), (SELECT COUNT(*) FROM meter_observation), (SELECT COUNT(*) FROM session_account_marker);"
}

case_assertions() {
    assert_exit 0 1
    assert_exit 0 2

    assert_exit 0 3
    assert_stdout_contains 3 "source_digest="
    assert_stdout_contains 3 "imported=2"
    assert_stdout_contains 3 "quarantined=1"
    assert_stderr_contains 3 "seed_archive_imported"
    assert_stderr_contains 3 "\"run\":\"run-"
    assert_stderr_contains 3 "\"source_digest\":"
    assert_stderr_contains 3 "\"verified_backup_id\":"
    assert_stderr_contains 3 "\"records_read\":{\"value\":3"
    assert_stderr_contains 3 "\"imported\":{\"value\":2"
    assert_stderr_contains 3 "\"unchanged\":{\"value\":0"
    assert_stderr_contains 3 "\"quarantined\":{\"value\":1"
    assert_stderr_contains 3 "\"terminal_outcome\":\"imported\""
    if grep -qF "$SOURCE" "$(step_dir 3)/stdout.txt" "$(step_dir 3)/stderr.txt"; then
        record_assertion "import output omits absolute source path" "absent" "present" "fail"
        CASE_FAILED=1
    else
        record_assertion "import output omits absolute source path" "absent" "absent" "pass"
    fi

    assert_exit 0 4
    assert_stdout_contains 4 "imported=0"
    assert_stdout_contains 4 "unchanged=2"

    assert_exit 0 5
    assert_stdout_contains 5 "records_read=1"
    assert_stdout_contains 5 "imported=0"
    assert_stdout_contains 5 "quarantined=1"

    assert_exit 5 6
    assert_stderr_contains 6 "backup"

    assert_exit 0 7
    assert_stdout_contains 7 "2|1|1"
}
