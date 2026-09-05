# aub-sth.12: `aub backup` against a populated state directory, run against the
# release binary because the property under test spans two process invocations
# writing and reading a real archive on disk, not one function call. The state
# directory is populated the same way case 011 populates one: a real
# `rate-card import` against the shipped fixture, which is the first production
# store user and therefore the cheapest way to get a migrated ledger on disk.
# The archive's file listing is captured as its own step precisely so the
# archive's contents are recorded in the run log, not just asserted about.

CASE_ID="012-backup"
CASE_DESCRIPTION="aub backup creates a verified archive of a populated state directory, and aub backup verify re-runs the same checks against it."

RATE_BOOK="$REPO_ROOT/tests/fixtures/rate-book/rates.toml"
ARCHIVE_DIR=""

aub_backup() {
    env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR/aub" \
        "$AUB_BIN" "$@"
}

case_preconditions() {
    require_command "$AUB_BIN"
    ARCHIVE_DIR="$STATE_DIR/aub-archive"
    if [ ! -r "$RATE_BOOK" ]; then
        record_assertion "rate book fixture readable" "present" "absent" "fail"
        CASE_FAILED=1
    fi
}

case_steps() {
    step "populate the ledger" \
        aub_backup rate-card import "$RATE_BOOK"
    step "create the backup archive" \
        aub_backup backup "$ARCHIVE_DIR"
    step "re-verify the existing archive" \
        aub_backup backup verify "$ARCHIVE_DIR"
    step "record the archive contents" \
        find "$ARCHIVE_DIR" -type f
}

case_assertions() {
    assert_exit 0 1
    assert_stdout_contains 1 "added=55"

    # The archive is verified the moment it is created: schema version 27 is
    # what the ledger migration chain produces today, and the ledger
    # generation starts at 0 because nothing meter-related has written yet.
    #
    # The trailing " generation=" is load-bearing, not decoration. These are
    # substring assertions, so a bare "schema=1" matched every version from 10
    # to 19 as well, and the check silently stopped verifying anything the
    # moment the chain passed nine. It was caught only when a bead added the
    # twentieth migration and "schema=20" finally failed to contain it. A bead
    # that adds a migration bumps this number, and now it will say so.
    assert_exit 0 2
    assert_stdout_contains 2 "verified=true"
    assert_stdout_contains 2 "schema=27 generation="
    assert_stdout_contains 2 "generation=0"
    assert_stdout_contains 2 "pending=0"
    assert_stdout_contains 2 "drain_completed=true"
    assert_stdout_contains 2 "destination=$ARCHIVE_DIR"

    # Re-verification against the same, untouched archive reports the same
    # result rather than a stale cached one.
    assert_exit 0 3
    assert_stdout_contains 3 "verified=true"
    assert_stdout_contains 3 "schema=27 generation="
    assert_stdout_contains 3 "generation=0"
    assert_stdout_contains 3 "pending=0"

    # The archive's own file inventory, recorded in the run log rather than
    # merely asserted about.
    assert_exit 0 4
    assert_stdout_contains 4 "ledger.db"
    assert_stdout_contains 4 "manifest.json"
    assert_stdout_contains 4 "checksums.sha256"
}
