# aub-lqe.18: bounded transcript ingest and the meter evidence cycle contend
# on the one ledger database, driven by the release binary. One step launches
# `aub ingest transcripts` and `__attempt-crash-hook sample` at the same time;
# a crash stage leaves a record spooled and uncommitted, which the concurrent
# sample run drains before its own attempts; the final read-back proves every
# attempt carries its terminal evidence exactly once, and the usage tables
# reconcile canonical events with occurrences one-to-one.

CASE_ID="014-ingest-meter-contention"
CASE_DESCRIPTION="Seeded ingestion and synthetic sampling run concurrently; meter evidence commits or spools, drains exactly once, and usage reconciles."

case_preconditions() {
    require_command "$AUB_BIN"
    require_command sqlite3

    # 240 distinct messages across two files: with max_batch_events = 60 the
    # ingest pass must split into exactly four bounded batches.
    local corpus="$STATE_DIR/transcripts/claude-code"
    mkdir -p "$corpus"
    local file message hour minute id
    for file in 0 1; do
        : > "$corpus/file$file.jsonl"
        for message in $(seq 0 119); do
            id=$(( file * 120 + message ))
            hour=$(( 10 + message / 60 + file * 2 ))
            minute=$(( message % 60 ))
            printf '{"type":"assistant","timestamp":"2026-08-25T%02d:%02d:00.000Z","sessionId":"s%d","message":{"id":"m%d","usage":{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}\n' \
                "$hour" "$minute" "$file" "$id" >> "$corpus/file$file.jsonl"
        done
    done

    CONFIG="$STATE_DIR/aub.toml"
    cat > "$CONFIG" <<EOT
[ingest]
max_batch_events = 60

[[transcripts]]
name = "claude-code"
root = "$corpus"
pattern = "**/*.jsonl"
format = "claude-code"
EOT
}

case_steps() {
    step "seed two meter attempts" env \
        "AUB_STATE_DIR=$STATE_DIR/aub" \
        "$AUB_BIN" __attempt-crash-hook sample --attempts 2
    step "row counts before the run" env \
        "AUB_STATE_DIR=$STATE_DIR/aub" \
        sqlite3 "$STATE_DIR/aub/ledger.db" \
        "SELECT (SELECT COUNT(*) FROM usage_event), (SELECT COUNT(*) FROM usage_occurrence), (SELECT COUNT(*) FROM meter_observation);"
    step "interrupt after spooling, before the commit" env \
        "AUB_STATE_DIR=$STATE_DIR/aub" \
        "$AUB_BIN" __attempt-crash-hook sample-crash
    step "run seeded ingestion and sampling concurrently" env \
        "AUB_BIN=$AUB_BIN" \
        "AUB_STATE_DIR=$STATE_DIR/aub" \
        "HOME=$STATE_DIR/home" \
        "AUB_CONFIG_FILE=$CONFIG" \
        "LOG_DIR=$STATE_DIR/logs" \
        bash -c '
      mkdir -p "$LOG_DIR"
      "$AUB_BIN" ingest transcripts -v >"$LOG_DIR/ingest.stdout" 2>"$LOG_DIR/ingest.stderr" &
      ingest_pid=$!
      "$AUB_BIN" __attempt-crash-hook sample --attempts 3 -v >"$LOG_DIR/sample.stdout" 2>"$LOG_DIR/sample.stderr" &
      sample_pid=$!
      wait "$ingest_pid"; ingest_rc=$?
      wait "$sample_pid"; sample_rc=$?
      echo "ingest_exit=$ingest_rc sample_exit=$sample_rc"
      echo "--- ingest stdout ---"
      cat "$LOG_DIR/ingest.stdout"
      echo "--- sample stdout ---"
      cat "$LOG_DIR/sample.stdout"
      echo "--- structured diagnostics (stderr, both runs) ---"
      cat "$LOG_DIR/ingest.stderr" "$LOG_DIR/sample.stderr" >&2
      [ "$ingest_rc" -eq 0 ] && [ "$sample_rc" -eq 0 ]
    '
    step "final sampling run drains any straggler" env \
        "AUB_STATE_DIR=$STATE_DIR/aub" \
        "$AUB_BIN" __attempt-crash-hook sample --attempts 1
    step "reconcile the ledger after the run" env \
        "AUB_STATE_DIR=$STATE_DIR/aub" \
        sqlite3 "$STATE_DIR/aub/ledger.db" \
        "SELECT (SELECT COUNT(*) FROM usage_event), (SELECT COUNT(*) FROM usage_occurrence), (SELECT COUNT(*) FROM usage_occurrence o JOIN usage_event e ON e.id = o.event_id), (SELECT COUNT(*) FROM meter_attempt), (SELECT COUNT(*) FROM meter_attempt_result), (SELECT COUNT(*) FROM meter_response_evidence), (SELECT COUNT(*) FROM meter_observation);"
}

case_assertions() {
    # Seeded attempts with no competing writer: both commit outright.
    assert_exit 0 1
    assert_stdout_contains 1 "sample attempts=2 committed=2 spooled=0"

    # Before the concurrent run: two observations, no usage rows at all.
    assert_exit 0 2
    assert_stdout_contains 2 "0|0|2"

    # The crash stage ends by signal: the record is durably spooled, the
    # terminal commit never ran, and no clean exit can be mistaken for it.
    assert_signal 6 3

    # The concurrent run: both processes exit 0, the pass splits into exactly
    # four bounded batches, and the sample run's startup drain applies the
    # interrupted record exactly once before its own three attempts.
    assert_exit 0 4
    assert_stdout_contains 4 "ingest_exit=0 sample_exit=0"
    assert_stdout_contains 4 "batches=4"
    assert_stdout_contains 4 "drain applied=1 already-applied=0 quarantined=0"
    # Contention may send an attempt to the spool under extreme machine
    # pressure; the read-back below proves the accounting either way.
    assert_stdout_contains 4 "sample attempts=3 committed="

    # Structured diagnostics correlate the two runs by stable identifiers:
    # run ids open each process, batches announce index, size, writer-slot
    # hold and generation, and the meter side names attempts, spool writes
    # and the drain.
    assert_stderr_contains 4 '"event":"run_started"'
    assert_stderr_contains 4 '"event":"ingest_batch_landed"'
    assert_stderr_contains 4 '"writer_slot"'
    assert_stderr_contains 4 '"generation"'
    assert_stderr_contains 4 '"event":"meter_attempt_committed"'
    assert_stderr_contains 4 '"busy_wait"'
    assert_stderr_contains 4 '"event":"meter_spool_drained"'
    assert_stderr_contains 4 '"already_applied"'

    # The final run drains anything still pending before its one attempt, so
    # the read-back below is deterministic regardless of how contention went.
    assert_exit 0 5
    assert_stdout_contains 5 "sample attempts=1"

    # The reconciliation after the run, one row of counts: canonical usage
    # equals occurrences equals linked occurrences (every identity landed
    # once, none orphaned), and every one of the seven attempts carries its
    # terminal result, evidence and observation exactly once. A lost or
    # duplicated bundle shows up here as any other number.
    assert_exit 0 6
    assert_stdout_contains 6 "240|240|240|7|7|7|7"
}