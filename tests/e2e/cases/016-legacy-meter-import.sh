# `aub import legacy-meter` crosses the administrative boundary deliberately:
# it requires an independently verified archive before it writes, emits only a
# content digest for the source, and keeps an exact rerun from manufacturing a
# second historical timeline.

CASE_ID="016-legacy-meter-import"
CASE_DESCRIPTION="Legacy meter JSONL imports only after backup verification, remains idempotent, quarantines malformed rows, and never prints the source path."

SOURCE=""
MALFORMED_SOURCE=""
ARCHIVE=""

aub_legacy_meter() {
    env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR/aub" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "$AUB_BIN" "$@"
}

case_preconditions() {
    require_command "$AUB_BIN"
    require_command sqlite3

    SOURCE="$STATE_DIR/legacy-meter.jsonl"
    MALFORMED_SOURCE="$STATE_DIR/malformed-legacy-meter.jsonl"
    ARCHIVE="$STATE_DIR/verified-archive"
    cat > "$STATE_DIR/aub.toml" <<EOF
state.dir = "$STATE_DIR/aub"

[[accounts]]
name = "primary"
provider = "anthropic"
EOF
    cat > "$SOURCE" <<'EOF'
{"ts":"2026-08-15T18:40:38Z","session_id":"legacy-a","account":"primary","tier":"pro","five_hour":28.000000000000004,"seven_day":44,"five_resets_at":"2026-08-15T20:00:00Z","seven_resets_at":"2026-08-22T00:00:00Z"}
{"ts":"2026-08-15T19:40:38Z","session_id":"legacy-b","account":"primary","tier":"pro","five_hour":31,"seven_day":45,"five_resets_at":"2026-08-15T21:00:00Z","seven_resets_at":"2026-08-22T00:00:00Z"}
not-json
EOF
    printf '%s\n' 'not-json' > "$MALFORMED_SOURCE"
}

case_steps() {
    step "initialize the ledger" aub_legacy_meter export --key run-id
    step "create a verified backup" aub_legacy_meter backup "$ARCHIVE"
    step "import the legacy source" aub_legacy_meter import legacy-meter --source "$SOURCE" --backup "$ARCHIVE" -v
    step "repeat the same import" aub_legacy_meter import legacy-meter --source "$SOURCE" --backup "$ARCHIVE"
    step "quarantine a malformed source" aub_legacy_meter import legacy-meter --source "$MALFORMED_SOURCE" --backup "$ARCHIVE"
    step "refuse an unverified backup" aub_legacy_meter import legacy-meter --source "$SOURCE" --backup "$STATE_DIR/not-an-archive"
    step "read durable import cardinalities" sqlite3 "$STATE_DIR/aub/ledger.db" "SELECT (SELECT COUNT(*) FROM legacy_meter_import_record), (SELECT COUNT(*) FROM meter_observation), (SELECT COUNT(*) FROM session_account_marker), (SELECT COUNT(*) FROM sample_run);"
    step "read the published projection through status" aub_legacy_meter status
}

case_assertions() {
    assert_exit 0 1
    assert_exit 0 2

    assert_exit 0 3
    assert_stdout_contains 3 "source_digest="
    assert_stdout_contains 3 "imported=2"
    assert_stdout_contains 3 "quarantined=1"
    assert_stderr_contains 3 "legacy_meter_imported"
    assert_stderr_contains 3 "\"run\":\"run-"
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
    assert_stdout_contains 7 "2|2|2|1"

    assert_exit 0 8
    assert_stdout_contains 8 "aub primary"
    assert_stdout_contains 8 "55%"
}
