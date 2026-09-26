# `aub import legacy-meter` crosses the administrative boundary deliberately:
# it requires an independently verified archive before it writes, emits only a
# content digest for the source, and keeps an exact rerun from manufacturing a
# second historical timeline.

CASE_ID="016-legacy-meter-import"
CASE_DESCRIPTION="Legacy meter JSONL imports only after backup verification, skips readings native sampling already covers while keeping their markers, quarantines malformed rows and unconfigured accounts, remains idempotent, and never prints the source path."

SOURCE=""
MALFORMED_SOURCE=""
STRADDLING_SOURCE=""
UNCONFIGURED_SOURCE=""
ARCHIVE=""

# The instant the straddling source is built around: the earliest native meter
# attempt planted below, in epoch seconds and in the nanoseconds the ledger
# stores. A reading one second earlier imports; the reading at exactly this
# instant does not.
NATIVE_CUTOFF_NANOS="1788523200000000000"

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

    STRADDLING_SOURCE="$STATE_DIR/straddling-legacy-meter.jsonl"
    cat > "$STRADDLING_SOURCE" <<'EOF'
{"ts":"2026-09-04T11:59:59Z","session_id":"straddle-before","account":"primary","tier":"pro","five_hour":10,"seven_day":20,"five_resets_at":"2026-09-04T15:00:00Z","seven_resets_at":"2026-09-08T00:00:00Z"}
{"ts":"2026-09-04T12:00:00Z","session_id":"straddle-at","account":"primary","tier":"pro","five_hour":11,"seven_day":21,"five_resets_at":"2026-09-04T15:00:00Z","seven_resets_at":"2026-09-08T00:00:00Z"}
{"ts":"2026-09-04T12:00:01Z","session_id":"straddle-after","account":"primary","tier":"pro","five_hour":12,"seven_day":22,"five_resets_at":"2026-09-04T15:00:00Z","seven_resets_at":"2026-09-08T00:00:00Z"}
EOF

    UNCONFIGURED_SOURCE="$STATE_DIR/unconfigured-legacy-meter.jsonl"
    cat > "$UNCONFIGURED_SOURCE" <<'EOF'
{"ts":"2026-08-16T09:00:00Z","session_id":"twin","account":"primary","tier":"pro","five_hour":5,"seven_day":6,"five_resets_at":"2026-08-16T10:00:00Z","seven_resets_at":"2026-08-22T00:00:00Z"}
{"ts":"2026-08-16T09:00:00Z","session_id":"twin","account":"file-auth:pro","tier":"pro","five_hour":5,"seven_day":6,"five_resets_at":"2026-08-16T10:00:00Z","seven_resets_at":"2026-08-22T00:00:00Z"}
EOF
}

# Plants one native meter attempt for `primary`, under a contract that is
# neither legacy one. Nothing offline can reach a provider endpoint, so the
# cutoff the importer must respect is written directly; what is under test is
# the importer's reading of it, not how the row got there.
plant_native_attempt() {
    sqlite3 "$STATE_DIR/aub/ledger.db" "
        INSERT INTO sample_run (trigger, started_at, aub_version, configuration_fingerprint)
            VALUES ('timer', $NATIVE_CUTOFF_NANOS, 'e2e-fixture', 'native-fixture-v1');
        INSERT INTO sampling_policy_snapshot (
            account_id, effective_at, ordinary_cadence_nanos, freshness_horizon_nanos,
            reset_edge_policy, retry_backoff_policy, command_budget_nanos, policy_algorithm_version
        ) SELECT id, $NATIVE_CUTOFF_NANOS, 600000000000, 600000000000,
                 'native-fixture', 'native-fixture', 5000000000, 'native-fixture-v1'
          FROM account WHERE provider_key = 'anthropic' AND logical_name = 'primary';
        INSERT INTO meter_attempt (
            run_id, account_id, provider, request_started_at, policy_snapshot_id,
            due_at, due_reason, provider_contract_id, meter_semantics_id
        ) SELECT (SELECT MAX(id) FROM sample_run), a.id, 'anthropic', $NATIVE_CUTOFF_NANOS,
                 (SELECT MAX(id) FROM sampling_policy_snapshot), $NATIVE_CUTOFF_NANOS,
                 'ordinary_cadence', 'anthropic-usage-endpoint-v1', 'native-account-windows-v1'
          FROM account a WHERE a.provider_key = 'anthropic' AND a.logical_name = 'primary';
    "
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
    step "plant a native meter attempt" plant_native_attempt
    step "import a straddling source" aub_legacy_meter import legacy-meter --source "$STRADDLING_SOURCE" --backup "$ARCHIVE"
    step "read the straddling markers" sqlite3 "$STATE_DIR/aub/ledger.db" "SELECT group_concat(session_native, ',') FROM (SELECT session_native FROM session_account_marker WHERE session_native LIKE 'straddle-%' ORDER BY session_native);"
    step "import an unconfigured account" aub_legacy_meter import legacy-meter --source "$UNCONFIGURED_SOURCE" --backup "$ARCHIVE"
    step "read the quarantine and the account table" sqlite3 "$STATE_DIR/aub/ledger.db" "SELECT parser, failure_class, line_number, (SELECT COUNT(*) FROM account WHERE logical_name = 'file-auth:pro') FROM ingest_quarantine WHERE failure_class = 'unconfigured_account';"
    step "describe the import rules" aub_legacy_meter import legacy-meter --help
}

case_assertions() {
    assert_exit 0 1
    assert_exit 0 2
    assert_stdout_contains 2 "backup: verified=true"

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
    assert_stderr_contains 6 "legacy import requires a verified backup archive"

    assert_exit 0 7
    assert_stdout_contains 7 "2|2|2|1"

    assert_exit 0 8
    assert_stdout_contains 8 "primary  anthropic"
    assert_stdout_contains 8 "45%"

    assert_exit 0 9

    # The cutoff is exclusive of itself: only the reading one second before the
    # planted native attempt may import.
    assert_exit 0 10
    assert_stdout_contains 10 "imported=1"
    assert_stdout_contains 10 "superseded_by_native=2"

    # Markers are exempt from the cutoff, so all three lines keep theirs.
    assert_exit 0 11
    assert_stdout_contains 11 "straddle-after,straddle-at,straddle-before"

    # The configured twin imports; its unconfigured near-identical partner does
    # not, and no account row is created for the name it carries.
    assert_exit 0 12
    assert_stdout_contains 12 "imported=1"
    assert_stdout_contains 12 "quarantined=1"

    assert_exit 0 13
    assert_stdout_contains 13 "legacy-meter|unconfigured_account|2|0"

    assert_exit 0 14
    assert_stdout_contains 14 "superseded_by_native"
    assert_stdout_contains 14 "exempt from that cutoff"
    assert_stdout_contains 14 "unconfigured_account"
}
