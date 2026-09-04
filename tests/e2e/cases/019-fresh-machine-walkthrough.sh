# aub-n27.6: the operator documentation's own "done when" criterion, run as a
# scripted walkthrough rather than asserted in prose. It follows
# docs/scheduling.md's "Bringing up a fresh machine" steps and docs/recovery.md's
# backup half in order, using only commands and invocation forms the shipped
# documentation actually shows: write aub.toml, confirm it resolved, run the
# scheduler tick and the documented hook invocation, read status, then create
# and verify a backup archive.
#
# The endpoint is unreachable (loopback port 9, as every other sampling case in
# this suite uses): there is no real provider to observe from inside the e2e
# sandbox. "A working sampling cadence" here means what the documentation
# actually promises for that case, scheduling.md's own words: a scheduled
# `sample --due` treats the remote failure as durably recorded evidence and
# exits zero, and status moves from the missing-projection question mark to a
# tracked account. It does not mean a successful reading, which needs a real
# provider docs/scheduling.md has no part in supplying.

CASE_ID="019-fresh-machine-walkthrough"
CASE_DESCRIPTION="Following the operator documentation alone (config, scheduler tick, hook, status, backup, verify) reaches a working sampling cadence and a verified backup."

CONFIG_FILE=""
ARCHIVE_DIR=""

case_preconditions() {
    require_command "$AUB_BIN"

    CONFIG_FILE="$STATE_DIR/aub.toml"
    ARCHIVE_DIR="$STATE_DIR/aub-archive"

    mkdir -p "$STATE_DIR/home" "$STATE_DIR/creds"
    echo '{"accessToken":"test-token"}' > "$STATE_DIR/creds/token.json"

    # README.md#configuration's shape: one [[accounts]] entry naming a
    # provider and a credential.
    cat > "$CONFIG_FILE" <<CFG_EOF
state.dir = "$STATE_DIR"

[[accounts]]
name = "work-primary"
provider = "anthropic"
credential = { kind = "file", path = "$STATE_DIR/creds/token.json" }
CFG_EOF
}

aub_walkthrough() {
    env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "$AUB_BIN" "$@"
}

case_steps() {
    # 1. First-run setup: the written file resolves, and aub config (the
    #    command docs/commands.md says answers "which configuration key
    #    resolved from where?") confirms it rather than requiring a read of
    #    the source to know the file was found.
    step "config-resolves-the-written-file" aub_walkthrough config

    # 2. Before any sampling, status reports the account as never observed:
    #    the starting state a fresh machine begins from.
    step "status-before-any-sample" aub_walkthrough status

    # 3. The scheduler tick docs/scheduling.md recommends: aub sample --due,
    #    against an endpoint nothing answers. The remote failure is durably
    #    recorded evidence, not a cadence failure. `env` execs a program, not
    #    a shell function, so the endpoint override is spelled out here
    #    rather than routed through aub_walkthrough.
    step "scheduler-tick" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "AUB_ANTHROPIC_ENDPOINT=http://127.0.0.1:9" \
        "$AUB_BIN" sample --due

    # 4. The documented hook invocation: fires moments later in the same
    #    cadence window, records the session/account marker without a second
    #    network attempt because the account was just sampled.
    step "hook-invocation" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "AUB_ANTHROPIC_ENDPOINT=http://127.0.0.1:9" \
        "$AUB_BIN" sample --account work-primary --if-due \
        --session-id "cli:fresh-machine-session" --run-id "cli:fresh-machine-run"

    # 5. Status now reads the projection the tick published: the account
    #    moved from never-observed to a tracked, if still stale, reading.
    step "status-after-the-tick" aub_walkthrough status

    # 6. The backup policy's first step: create a consistent, checksummed
    #    archive of the durable state the tick just wrote to.
    step "create-the-backup-archive" aub_walkthrough backup "$ARCHIVE_DIR"

    # 7. The backup policy's verification step: re-check the archive that
    #    now exists, independent of the process that created it.
    step "verify-the-backup-archive" aub_walkthrough backup verify "$ARCHIVE_DIR"
}

case_assertions() {
    # Step 1: config resolves the seeded file; state.dir shows the file source.
    assert_exit 0 1
    assert_stdout_contains 1 "state.dir"
    assert_stdout_contains 1 "file"

    # Step 2: nothing has been sampled yet, so no projection has ever been
    # published: the degraded question mark, not a per-account reading.
    assert_exit 0 2
    assert_stdout_contains 2 "aub ?"

    # Step 3: the scheduled tick exits zero even though the endpoint refused
    # the connection; the outcome is recorded, not swallowed.
    assert_exit 0 3
    assert_stdout_contains 3 "sample: account=work-primary outcome=unreachable"

    # Step 4: not due again inside the same window, so the hook records only
    # the marker and never reaches the network a second time.
    assert_exit 0 4
    assert_stdout_contains 4 "sample: account=work-primary not-due"

    # Step 5: status reads the published projection; the account is now
    # tracked (a recorded, if unreachable, attempt exists) instead of the
    # degraded question mark step 2 rendered before any sample ran.
    assert_exit 0 5
    assert_stdout_contains 5 "aub work-primary ? · stale · timeout"

    # Step 6: the archive is created and verified the moment it is written.
    assert_exit 0 6
    assert_stdout_contains 6 "verified=true"
    assert_stdout_contains 6 "destination=$ARCHIVE_DIR"

    # Step 7: independent re-verification agrees.
    assert_exit 0 7
    assert_stdout_contains 7 "verified=true"
}
