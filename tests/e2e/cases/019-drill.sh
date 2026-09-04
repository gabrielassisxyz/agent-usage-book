# aub-n27.2: aub drill damages a scratch state directory in each of the four
# documented ways and drives the same recovery procedure docs/recovery.md
# tells an operator to follow by hand. A fifth step proves the refusal covers
# the configured state directory, and a sixth runs the drill against a real
# archive the way a scheduler would, whose durable result then shows up
# through aub doctor's backup-age check without anyone reading a log by hand.

CASE_ID="019-drill"
CASE_DESCRIPTION="aub drill recovers all four seeded damage cases, refuses the configured state directory in both modes, and a scheduler-style archive run becomes visible in aub doctor."

CONFIG_FILE=""
INNER_STATE_DIR=""
ARCHIVE_DIR=""
DRILL_RESULT=""

aub_drill() {
    env \
        "HOME=$STATE_DIR/home" \
        "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "$AUB_BIN" "$@"
}

case_preconditions() {
    require_command "$AUB_BIN"
    INNER_STATE_DIR="$STATE_DIR/state"
    ARCHIVE_DIR="$STATE_DIR/archive"
    DRILL_RESULT="$STATE_DIR/drill-result.jsonl"
    CONFIG_FILE="$STATE_DIR/aub.toml"
    cat > "$CONFIG_FILE" <<EOT
[state]
dir = "$INNER_STATE_DIR"

[backup]
destination = "$ARCHIVE_DIR"

[drill]
result = "$DRILL_RESULT"
max_age = "24h"
EOT
    mkdir -p "$INNER_STATE_DIR"
    # A migrated ledger at the configured state directory: rebuild transcripts
    # opens and migrates it even with no transcript source configured, the
    # same precondition 013-doctor uses.
    env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG_FILE" "$AUB_BIN" rebuild transcripts \
        >/dev/null 2>&1 || true
    env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG_FILE" "$AUB_BIN" backup "$ARCHIVE_DIR" \
        >/dev/null 2>&1 || true
}

case_steps() {
    step "drill: truncated database" \
        aub_drill drill --seed truncated-database "$STATE_DIR/drill-truncated"
    step "drill: corrupted projection" \
        aub_drill drill --seed corrupted-projection "$STATE_DIR/drill-projection"
    step "drill: malformed spool record" \
        aub_drill drill --seed malformed-spool-record "$STATE_DIR/drill-spool"
    step "drill: unsupported schema version" \
        aub_drill drill --seed unsupported-schema-version "$STATE_DIR/drill-schema"
    step "drill: refuses the configured state directory as scratch destination" \
        aub_drill drill --seed truncated-database "$INNER_STATE_DIR"
    step "drill: refuses the configured state directory as an archive source" \
        aub_drill drill --archive "$INNER_STATE_DIR" "$STATE_DIR/drill-archive-refused"
    step "drill: scheduler-style run against the real archive" \
        aub_drill drill --archive "$ARCHIVE_DIR" "$STATE_DIR/drill-scheduled"
    step "doctor: sees the fresh backup and the fresh drill" \
        aub_drill doctor
}

case_assertions() {
    # Case 1: truncated database. Recovery never opens the surviving
    # directory's own ledger, so it comes out byte-identical.
    assert_exit 0 1
    assert_stdout_contains 1 "drill: source=seeded:truncated-database"
    assert_stdout_contains 1 "drill: damaged_directory_preserved=true"
    assert_stdout_contains 1 "drill: passed=true"

    # Case 2: corrupted projection. Rebuilt, not restored, and deterministic.
    assert_exit 0 2
    assert_stdout_contains 2 "drill: source=seeded:corrupted-projection"
    assert_stdout_contains 2 "projection: rebuilt"
    assert_stdout_contains 2 "drill: damaged_directory_preserved=true"
    assert_stdout_contains 2 "drill: projection_deterministic=true"
    assert_stdout_contains 2 "drill: passed=true"

    # Case 3: malformed spool record. Reported as unrecovered, not fatal.
    assert_exit 0 3
    assert_stdout_contains 3 "drill: source=seeded:malformed-spool-record"
    assert_stdout_contains 3 "unrecovered=1"
    assert_stdout_contains 3 "unrecovered: surviving attempt-9999.json"
    assert_stdout_contains 3 "drill: damaged_directory_preserved=true"
    assert_stdout_contains 3 "drill: passed=true"

    # Case 4: unsupported schema version. Never opened, so nothing to refuse.
    assert_exit 0 4
    assert_stdout_contains 4 "drill: source=seeded:unsupported-schema-version"
    assert_stdout_contains 4 "drill: damaged_directory_preserved=true"
    assert_stdout_contains 4 "drill: passed=true"

    # The refusal covers both the scratch-destination argument and the
    # archive-source argument, in the same way for both damage-case and
    # real-archive modes.
    assert_exit 5 5
    assert_stderr_contains 5 "configured state directory"
    assert_exit 5 6
    assert_stderr_contains 6 "configured state directory"

    # The periodic case: a real archive, restored and checked the same way.
    assert_exit 0 7
    assert_stdout_contains 7 "drill: source=archive:$ARCHIVE_DIR"
    assert_stdout_contains 7 "drill: passed=true"

    # doctor sees both the backup this run created and the drill this run
    # just recorded, with no operator having to read a log by hand.
    assert_exit 0 8
    assert_stdout_contains 8 "[PASS] backup-age"
}
