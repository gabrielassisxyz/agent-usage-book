# `aub import seed-archive` crosses the administrative boundary deliberately:
# it requires an independently verified archive before it writes, emits only a
# content digest for the source, and keeps an exact rerun from manufacturing a
# second historical timeline.

CASE_ID="018-seed-archive-import"
CASE_DESCRIPTION="Seed archive imports only after backup verification and only what native sampling does not cover, remains idempotent, quarantines malformed rows, discards a vendor it has no account for, and never prints the source path."

SOURCE=""
MALFORMED_SOURCE=""
STRADDLING_SOURCE=""
MULTI_VENDOR_SOURCE=""
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
    STRADDLING_SOURCE="$STATE_DIR/straddling-seed.jsonl"
    MULTI_VENDOR_SOURCE="$STATE_DIR/multi-vendor-seed.jsonl"
    ARCHIVE="$STATE_DIR/verified-archive"
    cat > "$STATE_DIR/aub.toml" <<EOF
state.dir = "$STATE_DIR/aub"

[[accounts]]
name = "primary"
provider = "anthropic"
EOF
    cat > "$SOURCE" <<'JSONL'
{"received_at":"2026-08-26T03:00:00Z","account":"claude","tool":"aub-meter","tool_version":"0.1.0","plan":"pro","reading":{"generatedAt":"2026-08-26T02:59:58Z","providers":[{"provider":"claude","windows":[{"id":"five_hour","percentUsed":10,"resetsAt":"2026-08-26T05:00:00Z","windowSeconds":18000},{"id":"seven_day","percentUsed":20,"resetsAt":"2026-09-02T00:00:00Z","windowSeconds":604800}]}]}}
{"received_at":"2026-08-26T03:06:00Z","account":"claude","tool":"aub-meter","tool_version":"0.1.0","failure":"spawn_failed","exit_code":1}
not-json
JSONL
    printf '%s\n' 'not-json' > "$MALFORMED_SOURCE"
    # One reading before the native attempt this case plants, one after it.
    cat > "$STRADDLING_SOURCE" <<'JSONL'
{"received_at":"2026-08-26T02:48:00Z","account":"claude","tool":"aub-meter","tool_version":"0.1.0","plan":"pro","reading":{"generatedAt":"2026-08-26T02:47:58Z","providers":[{"provider":"claude","windows":[{"id":"five_hour","percentUsed":8,"resetsAt":"2026-08-26T05:00:00Z","windowSeconds":18000}]}]}}
{"received_at":"2026-08-26T03:18:00Z","account":"claude","tool":"aub-meter","tool_version":"0.1.0","plan":"pro","reading":{"generatedAt":"2026-08-26T03:17:58Z","providers":[{"provider":"claude","windows":[{"id":"five_hour","percentUsed":12,"resetsAt":"2026-08-26T05:00:00Z","windowSeconds":18000}]}]}}
JSONL
    # One line, four vendors, and only claude is mapped on the command line.
    cat > "$MULTI_VENDOR_SOURCE" <<'JSONL'
{"received_at":"2026-08-26T02:42:00Z","account":"claude","tool":"aub-meter","tool_version":"0.1.0","plan":"pro","reading":{"generatedAt":"2026-08-26T02:41:58Z","providers":[{"provider":"claude","windows":[{"id":"five_hour","percentUsed":7,"resetsAt":"2026-08-26T05:00:00Z","windowSeconds":18000}]},{"provider":"codex","windows":[{"id":"five_hour","percentUsed":6,"resetsAt":"2026-08-26T05:00:00Z","windowSeconds":18000}]},{"provider":"cursor","windows":[{"id":"five_hour","percentUsed":5,"resetsAt":"2026-08-26T05:00:00Z","windowSeconds":18000}]},{"provider":"kimi","windows":[{"id":"five_hour","percentUsed":4,"resetsAt":"2026-08-26T05:00:00Z","windowSeconds":18000}]}]}}
JSONL
}

case_steps() {
    step "initialize the ledger" aub_seed export --key run-id
    step "create a verified backup" aub_seed backup "$ARCHIVE"
    step "import the seed archive source" aub_seed import seed-archive --source "$SOURCE" --backup "$ARCHIVE" --vendor-account claude=primary -v
    step "repeat the same import" aub_seed import seed-archive --source "$SOURCE" --backup "$ARCHIVE" --vendor-account claude=primary
    step "quarantine a malformed source" aub_seed import seed-archive --source "$MALFORMED_SOURCE" --backup "$ARCHIVE" --vendor-account claude=primary
    step "refuse an unverified backup" aub_seed import seed-archive --source "$SOURCE" --backup "$STATE_DIR/not-an-archive" --vendor-account claude=primary
    step "read durable import cardinalities" sqlite3 "$STATE_DIR/aub/ledger.db" "SELECT (SELECT COUNT(*) FROM meter_attempt), (SELECT COUNT(*) FROM meter_observation), (SELECT COUNT(*) FROM session_account_marker);"
    step "refuse a vendor mapped to an account no [[accounts]] entry names" aub_seed import seed-archive --source "$SOURCE" --backup "$ARCHIVE" --vendor-account claude=not-configured
    step "discard a vendor with no mapping" aub_seed import seed-archive --source "$MULTI_VENDOR_SOURCE" --backup "$ARCHIVE" --vendor-account claude=primary
    # Native coverage cannot be produced here without reaching a provider, so the
    # attempt is planted directly, reusing the rows the first import created. The
    # contract is a native one, which is the only property the cutoff reads.
    step "plant a native meter attempt" sqlite3 "$STATE_DIR/aub/ledger.db" "INSERT INTO meter_attempt (run_id, account_id, provider, request_started_at, policy_snapshot_id, due_at, due_reason, provider_contract_id, meter_semantics_id) SELECT (SELECT id FROM sample_run ORDER BY id LIMIT 1), (SELECT id FROM account ORDER BY id LIMIT 1), 'anthropic', CAST(strftime('%s','2026-08-26 03:03:00') AS INTEGER) * 1000000000, (SELECT id FROM sampling_policy_snapshot ORDER BY id LIMIT 1), CAST(strftime('%s','2026-08-26 03:03:00') AS INTEGER) * 1000000000, 'ordinary_cadence', 'anthropic-oauth-usage-limits-v1', 'native-e2e-semantics-v1';"
    step "import only the readings before the native cutoff" aub_seed import seed-archive --source "$STRADDLING_SOURCE" --backup "$ARCHIVE" --vendor-account claude=primary
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

    assert_exit 2 8
    assert_stderr_contains 8 "not-configured"

    assert_exit 0 9
    assert_stdout_contains 9 "imported=1"
    assert_stdout_contains 9 "discarded_unmapped_vendor=3"

    assert_exit 0 10

    assert_exit 0 11
    assert_stdout_contains 11 "records_read=2"
    assert_stdout_contains 11 "imported=1"
    assert_stdout_contains 11 "superseded_by_native=1"
}
