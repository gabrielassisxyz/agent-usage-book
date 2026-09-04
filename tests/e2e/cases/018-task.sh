# `aub task ingest` then `aub task report` then `aub task overhead` over one
# seeded corpus (`aub-eu7.4`): the tracker's one claim boundary splits two
# transcript events into a task-attributed one and an overhead one, and the
# task report and overhead report reconcile to the same totals through the
# real binary.

CASE_ID="018-task"
CASE_DESCRIPTION="task ingest, report and overhead reconcile per token kind over a seeded corpus."

CONFIG=""
TRACKER_DB=""

case_preconditions() {
    require_command sqlite3

    local corpus="$STATE_DIR/transcripts/claude-code"
    mkdir -p "$corpus"
    # m1 occurs before the claim below and lands in the before_first_claim
    # overhead bucket; m2 occurs after it and is attributed to the task.
    cat > "$corpus/session.jsonl" <<'JSONL'
{"type":"assistant","timestamp":"2026-08-25T09:00:00.000Z","sessionId":"s1","message":{"id":"m1","usage":{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}
{"type":"assistant","timestamp":"2026-08-25T11:00:00.000Z","sessionId":"s1","message":{"id":"m2","usage":{"input_tokens":20,"output_tokens":8,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}
JSONL

    local tracker_dir="$STATE_DIR/tracker"
    mkdir -p "$tracker_dir"
    TRACKER_DB="$tracker_dir/beads.db"
    sqlite3 "$TRACKER_DB" <<'SQL'
CREATE TABLE events (
    id INTEGER PRIMARY KEY,
    issue_id TEXT NOT NULL,
    event_type TEXT NOT NULL,
    actor TEXT,
    old_value TEXT,
    new_value TEXT,
    created_at TEXT NOT NULL
);
INSERT INTO events (id, issue_id, event_type, actor, old_value, new_value, created_at)
VALUES (1, 'aub-1', 'status_changed', 'agent-1', 'open', 'in_progress', '2026-08-25T10:00:00Z');
SQL

    CONFIG="$STATE_DIR/aub.toml"
    cat > "$CONFIG" <<EOT
[[transcripts]]
name = "claude-code"
root = "$corpus"
pattern = "**/*.jsonl"
format = "claude-code"

[tracker]
kind = "local"
path = "$tracker_dir"
EOT
}

case_steps() {
    step "ingest transcripts" env \
        "HOME=$STATE_DIR/home" \
        "AUB_CONFIG_FILE=$CONFIG" \
        "$AUB_BIN" ingest transcripts
    step "task ingest" env \
        "HOME=$STATE_DIR/home" \
        "AUB_CONFIG_FILE=$CONFIG" \
        "$AUB_BIN" task ingest
    step "task report" env \
        "HOME=$STATE_DIR/home" \
        "AUB_CONFIG_FILE=$CONFIG" \
        "$AUB_BIN" task report beads:aub-1
    step "task overhead" env \
        "HOME=$STATE_DIR/home" \
        "AUB_CONFIG_FILE=$CONFIG" \
        "$AUB_BIN" task overhead --since 2026-08-25 --days 1
}

case_assertions() {
    assert_exit 0 1

    assert_exit 0 2
    assert_stdout_contains 2 "task ingest: events_inserted=1 events_already_present=0 quarantines_inserted=0 quarantines_already_present=0"

    # Only the post-claim event (m2) is attributed to the task; the
    # pre-claim event (m1) must not appear in its total.
    assert_exit 0 3
    assert_stdout_contains 3 "task beads:aub-1"
    assert_stdout_contains 3 "input 20 tokens"
    assert_stdout_contains 3 "output 8 tokens"
    assert_stdout_contains 3 "task kind: no tracker evidence"

    # The overhead report shows the same task-attributed total the report
    # command computed independently, plus the before_first_claim bucket
    # holding exactly what the task total excluded, at 100% share (the
    # only bucket present).
    assert_exit 0 4
    assert_stdout_contains 4 "task-attributed usage: input 20 tokens"
    assert_stdout_contains 4 "output 8 tokens"
    assert_stdout_contains 4 "before_first_claim"
    assert_stdout_contains 4 "100% share"
    assert_stdout_contains 4 "input 10 tokens"
    assert_stdout_contains 4 "output 5 tokens"
}
