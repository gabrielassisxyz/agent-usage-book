# aub-sth.13: the first restore drill uses the release binary against a real
# archive and two pending-evidence sources. The test hook writes complete
# observation bundles through the normal repository boundary, not raw SQL.
# An orphan pending record sorts before the valid one, leaving both in the
# archive; replay then proves a duplicate valid record is counted once and an
# unrecoverable record is named from each source.

CASE_ID="013-restore-drill"
CASE_DESCRIPTION="aub backup restore preserves the damaged spool, replays two pending sources idempotently, and reports unrecovered evidence."

ARCHIVE_DIR=""
RESTORED_DIR=""
SURVIVING_DIR=""

aub_restore_drill() {
    env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR/aub" \
        "$AUB_BIN" "$@"
}

aub_restore_from_damaged_state() {
    env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$SURVIVING_DIR" \
        "$AUB_BIN" "$@"
}

case_preconditions() {
    require_command "$AUB_BIN"
    ARCHIVE_DIR="$STATE_DIR/archive"
    RESTORED_DIR="$STATE_DIR/restored"
    SURVIVING_DIR="$STATE_DIR/surviving"
}

case_steps() {
    step "seed first committed observation" aub_restore_drill __attempt-crash-hook commit-observation
    step "seed second committed observation" aub_restore_drill __attempt-crash-hook commit-observation
    step "seed replayable pending observation" aub_restore_drill __attempt-crash-hook spool-pending
    step "seed unrecoverable pending observation" aub_restore_drill __attempt-crash-hook spool-orphan 0
    step "create archive with pending evidence" aub_restore_drill backup "$ARCHIVE_DIR"
    step "copy the damaged spool for read-only replay" cp -a "$STATE_DIR/aub" "$SURVIVING_DIR"
    step "restore archive and replay both sources" \
        aub_restore_from_damaged_state backup restore "$ARCHIVE_DIR" "$RESTORED_DIR" --surviving "$SURVIVING_DIR"
    step "confirm the damaged spool is unchanged" \
        test -f "$SURVIVING_DIR/pending/attempt-0.json"
    step "confirm no damaged record was quarantined" \
        test ! -e "$SURVIVING_DIR/pending/quarantine"
    step "refuse configured state directory as destination" \
        aub_restore_drill backup restore "$ARCHIVE_DIR" "$STATE_DIR/aub"
}

case_assertions() {
    assert_exit 0 1
    assert_stdout_contains 1 "committed=1"
    assert_exit 0 2
    assert_stdout_contains 2 "committed=2"
    assert_exit 0 3
    assert_stdout_contains 3 "spooled=3"
    assert_exit 0 4
    assert_stdout_contains 4 "spooled-orphan=0"

    assert_exit 0 5
    assert_stdout_contains 5 "verified=true"
    assert_stdout_contains 5 "pending=2"
    assert_stdout_contains 5 "drain_completed=false"

    assert_exit 0 6
    assert_exit 0 7
    assert_stdout_contains 7 "archive_verified=true"
    assert_stdout_contains 7 "pending_restored=2"
    assert_stdout_contains 7 "observations=3"
    assert_stdout_contains 7 "unrecovered=2"
    assert_stdout_contains 7 "integrity=ok foreign_keys=ok"
    assert_stdout_contains 7 "replay: source=archive applied=1 already_applied=0 quarantined=1"
    assert_stdout_contains 7 "replay: source=surviving applied=0 already_applied=1 quarantined=1"
    assert_stdout_contains 7 "unrecovered: archive attempt-0.json"
    assert_stdout_contains 7 "unrecovered: surviving attempt-0.json"
    assert_stdout_contains 7 "projection: rebuilt"

    assert_exit 0 8
    assert_exit 0 9
    assert_exit 5 10
    assert_stderr_contains 10 "configured state directory"
}
