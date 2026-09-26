# aub-og69: `aub can-run <bead-id>` derives the routing cell from the bead's
# own identity row instead of asking the caller to name the group by hand.
#
# Two trackers share the native id `aub-1`, so a bare `aub-1` is ambiguous and
# must name both trackers and ask for `<source>/<id>`; `aub-9` lives in one
# tracker only and carries an unlabeled identity, so the bead form answers in
# the conservative default cell with the same verdict as the by-hand
# `--task-kind` form. `aub-7` carries a labeled identity in another cell and
# proves the reported cell follows the labels. `aub-2` resolves but has no
# identity row, and `aub-nope` resolves nowhere.
#
# Only the two positive runs need a meter reading: every usage error fails
# before sampling, so those steps run with no server at all. The one fresh
# sample answers against the stub server, which is killed before the `--cached`
# steps prove they read no network.

CASE_ID="aub-og69-can-run-from-a-bead-id"
CASE_DESCRIPTION="can-run answers for a bead id with the ledger-derived cell on both surfaces, refuses the ambiguous, missing and mixed forms as usage errors, and agrees with the by-hand form in the default cell."

CONFIG=""
LEDGER_DB=""
SERVER_PID=""
PORT=""

case_preconditions() {
    require_command "$AUB_BIN"
    require_command sqlite3
    require_command python3
    require_command jq

    LEDGER_DB="$STATE_DIR/ledger.db"
    CONFIG="$STATE_DIR/aub.toml"

    mkdir -p "$STATE_DIR/home" "$STATE_DIR/creds" \
        "$STATE_DIR/transcripts/claude-code" "$STATE_DIR/tracker-a" "$STATE_DIR/tracker-b"
    echo '{"accessToken":"test-token"}' > "$STATE_DIR/creds/token.json"

    cat > "$CONFIG" <<CFG_EOF
state.dir = "$STATE_DIR"

[[accounts]]
name = "work-primary"
provider = "anthropic"
credential = { kind = "file", path = "$STATE_DIR/creds/token.json" }

[task_distribution]
min_samples = 1

[[transcripts]]
name = "claude-code"
root = "$STATE_DIR/transcripts/claude-code"
pattern = "**/*.jsonl"
format = "claude-code"

[[trackers]]
name = "repo-a"
kind = "local"
path = "$STATE_DIR/tracker-a"

[[trackers]]
name = "repo-b"
kind = "local"
path = "$STATE_DIR/tracker-b"
CFG_EOF

    # One session with one usage record: the only priced task history.
    cat > "$STATE_DIR/transcripts/claude-code/session.jsonl" <<'JSONL'
{"type":"assistant","timestamp":"2026-08-25T01:00:00.000Z","sessionId":"s9","message":{"id":"m9","usage":{"input_tokens":1000,"output_tokens":5000,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}
JSONL

    # repo-a holds aub-1 (also in repo-b: the ambiguous id) and aub-2 (which
    # resolves but never gets an identity row).
    sqlite3 "$STATE_DIR/tracker-a/beads.db" <<'SQL'
CREATE TABLE issues (
    id TEXT PRIMARY KEY,
    issue_type TEXT NOT NULL DEFAULT 'task'
);
CREATE TABLE events (
    id INTEGER PRIMARY KEY,
    issue_id TEXT NOT NULL,
    event_type TEXT NOT NULL,
    actor TEXT,
    old_value TEXT,
    new_value TEXT,
    created_at TEXT NOT NULL
);
INSERT INTO issues (id, issue_type) VALUES ('aub-1', 'task'), ('aub-2', 'task');
INSERT INTO events (id, issue_id, event_type, actor, old_value, new_value, created_at) VALUES
 (1, 'aub-1', 'status_changed', 'agent-1', 'open', 'in_progress', '2026-08-25T03:00:00Z'),
 (2, 'aub-1', 'status_changed', 'agent-1', 'in_progress', 'closed', '2026-08-25T04:00:00Z'),
 (3, 'aub-2', 'status_changed', 'agent-1', 'open', 'in_progress', '2026-08-25T05:00:00Z'),
 (4, 'aub-2', 'status_changed', 'agent-1', 'in_progress', 'closed', '2026-08-25T06:00:00Z');
SQL

    # repo-b holds aub-1 (the other half of the ambiguity), aub-9 (the
    # positive bead, claimed over the one usage record) and aub-7 (the
    # labeled bead in another cell, with no usage of its own).
    sqlite3 "$STATE_DIR/tracker-b/beads.db" <<'SQL'
CREATE TABLE issues (
    id TEXT PRIMARY KEY,
    issue_type TEXT NOT NULL DEFAULT 'task'
);
CREATE TABLE events (
    id INTEGER PRIMARY KEY,
    issue_id TEXT NOT NULL,
    event_type TEXT NOT NULL,
    actor TEXT,
    old_value TEXT,
    new_value TEXT,
    created_at TEXT NOT NULL
);
INSERT INTO issues (id, issue_type) VALUES ('aub-1', 'task'), ('aub-9', 'task'), ('aub-7', 'task');
INSERT INTO events (id, issue_id, event_type, actor, old_value, new_value, created_at) VALUES
 (1, 'aub-1', 'status_changed', 'agent-1', 'open', 'in_progress', '2026-08-25T07:00:00Z'),
 (2, 'aub-1', 'status_changed', 'agent-1', 'in_progress', 'closed', '2026-08-25T08:00:00Z'),
 (3, 'aub-9', 'status_changed', 'agent-1', 'open', 'in_progress', '2026-08-25T00:30:00Z'),
 (4, 'aub-9', 'status_changed', 'agent-1', 'in_progress', 'closed', '2026-08-25T02:00:00Z'),
 (5, 'aub-7', 'status_changed', 'agent-1', 'open', 'in_progress', '2026-08-25T09:00:00Z'),
 (6, 'aub-7', 'status_changed', 'agent-1', 'in_progress', 'closed', '2026-08-25T10:00:00Z');
SQL

    python3 -c "
import http.server, socketserver, sys
port_file, fixture = sys.argv[1], sys.argv[2]
class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        with open(fixture, 'rb') as f:
            data = f.read()
        self.send_response(200)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(data)))
        self.end_headers()
        self.wfile.write(data)
    def log_message(self, format, *args):
        pass
httpd = socketserver.TCPServer(('127.0.0.1', 0), Handler)
with open(port_file, 'w') as pf:
    pf.write(str(httpd.server_address[1]))
httpd.serve_forever()
" "$STATE_DIR/port.txt" "$REPO_ROOT/tests/fixtures/meter/anthropic/can-run-worked-example.json" &
    SERVER_PID=$!

    local count=0
    while [ ! -s "$STATE_DIR/port.txt" ]; do
        sleep 0.05
        count=$((count + 1))
        if [ "$count" -gt 60 ]; then
            echo "timed out waiting for stub server" >&2
            exit 1
        fi
    done
    PORT=$(cat "$STATE_DIR/port.txt")
}

case_steps() {
    step "ingest-transcripts" env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG" \
        "$AUB_BIN" ingest transcripts
    step "task-ingest" env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG" \
        "$AUB_BIN" task ingest

    # Identity rows are seeded directly: no CLI resolves a task's routing
    # axes yet, matching this suite's convention for tables with no
    # ingestion path of their own. aub-9 stays unlabeled (the default cell),
    # aub-7 is labeled into another cell, and aub-2 gets no row at all.
    step "seed-task-identity" sqlite3 "$LEDGER_DB" "
        INSERT INTO task_identity (
            task_source, task_native, state, kind, winner_origin, evidence,
            normalization_version, size_state, size, size_evidence,
            difficulty_state, difficulty, difficulty_evidence,
            verify_state, verify, verify_evidence, spec_state, spec, spec_evidence
        ) VALUES
         ('repo-b', 'aub-9', 'resolved', 'task', 'tracker_field:kind', '{}', 1, 'unknown', NULL, '{}', 'unknown', NULL, '{}', 'unknown', NULL, '{}', 'unknown', NULL, '{}'),
         ('repo-b', 'aub-7', 'resolved', 'task', 'tracker_field:kind', '{}', 1, 'resolved', 'XL', '{}', 'unknown', NULL, '{}', 'resolved', 'external', '{}', 'resolved', 'open', '{}');
    "

    step "seed-account-marker" sqlite3 "$LEDGER_DB" "
        INSERT INTO session_account_marker
            (session_source, session_native, observed_at, source_ordering_key,
             logical_account, resolved_account_id, marker_source, run_source,
             run_native, evidence_designation)
        VALUES
         ('claude-code', 's9', 1787617800000000000, NULL, 'work-primary', NULL, 'hook', NULL, NULL, 'launcher_or_hook');
    "

    step "seed-cost-model" env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG" \
        "$AUB_BIN" cost-model activate anthropic_claude_messages_v1
    step "seed-calibration-five-hour" env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG" \
        "$AUB_BIN" __calibration-fixture five_hour 100
    step "seed-calibration-seven-day" env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG" \
        "$AUB_BIN" __calibration-fixture seven_day 100
    step "seed-calibration-seven-day-sonnet" env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG" \
        "$AUB_BIN" __calibration-fixture seven_day_sonnet 40

    # The bead form, fresh: samples once against the stub server, then
    # advises for aub-9 in the default cell.
    step "can-run-bead" env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG" \
        "AUB_ANTHROPIC_ENDPOINT=http://127.0.0.1:$PORT" \
        "$AUB_BIN" can-run aub-9 --account work-primary --task-model sonnet

    kill "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
    SERVER_PID=""

    # The by-hand form over the same history: aub-9 is unlabeled, so both
    # forms compare against the same default cell and agree.
    step "can-run-task-kind" env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG" \
        "$AUB_BIN" can-run --task-kind task --account work-primary --task-model sonnet --cached

    # The labeled bead in another cell: its own cell is empty, so the answer
    # falls back to the breadth parent and names it.
    step "can-run-labeled-bead" env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG" \
        "$AUB_BIN" can-run aub-7 --account work-primary --task-model sonnet --cached

    step "can-run-bead-json" env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG" \
        "$AUB_BIN" can-run aub-9 --account work-primary --task-model sonnet --cached --format json

    # Usage errors: all fail before sampling, with the server long dead.
    step "can-run-bead-and-kind" env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG" \
        "$AUB_BIN" can-run aub-9 --task-kind task --account work-primary --task-model sonnet --cached
    step "can-run-no-match" env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG" \
        "$AUB_BIN" can-run aub-nope --account work-primary --task-model sonnet --cached
    step "can-run-ambiguous" env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG" \
        "$AUB_BIN" can-run aub-1 --account work-primary --task-model sonnet --cached
    step "can-run-qualified-missing-identity" env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG" \
        "$AUB_BIN" can-run repo-a/aub-1 --account work-primary --task-model sonnet --cached
    step "can-run-missing-identity" env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG" \
        "$AUB_BIN" can-run aub-2 --account work-primary --task-model sonnet --cached
    step "can-run-help" env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG" \
        "$AUB_BIN" can-run --help
}

case_assertions() {
    if [ -n "${SERVER_PID:-}" ]; then
        kill "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
    fi

    assert_exit 0 1
    assert_exit 0 2
    assert_exit 0 3
    assert_exit 0 4
    assert_exit 0 5
    assert_exit 0 6
    assert_exit 0 7
    assert_exit 0 8

    # The bead form states the derived cell and its source bead id.
    assert_exit 0 9
    assert_stdout_contains 9 "can-run: task"
    assert_stdout_contains 9 "bead: repo-b/aub-9"
    assert_stdout_contains 9 "history: level=cell group=l/gate/open critical=false"
    assert_stdout_contains 9 "assessment: AMPLE"

    # The by-hand form over the same history agrees: same cell, same verdict.
    assert_exit 0 10
    assert_stdout_contains 10 "history: level=cell group=l/gate/open critical=false"
    assert_stdout_contains 10 "assessment: AMPLE"

    # The labeled bead reports its own lineage: an empty cell, so the
    # breadth parent answers and is named as the level.
    assert_exit 0 11
    assert_stdout_contains 11 "bead: repo-b/aub-7"
    assert_stdout_contains 11 "history: level=breadth_critical group=l critical=false"

    assert_exit 0 12
    assert_json_field 12 command can-run
    assert_json_field 12 bead_id repo-b/aub-9
    assert_json_field 12 history_level cell
    assert_json_field 12 history_group.breadth l
    assert_json_field 12 history_group.verify gate
    assert_json_field 12 history_group.spec open

    # A bead id and --task-kind together name both.
    assert_exit 2 13
    assert_stderr_contains 13 "aub-9"
    assert_stderr_contains 13 "--task-kind"

    # No tracker holds the id: the error names the trackers searched.
    assert_exit 2 14
    assert_stderr_contains 14 "aub-nope"
    assert_stderr_contains 14 "repo-a"
    assert_stderr_contains 14 "repo-b"

    # Both trackers hold the id: the error names both and asks for the
    # qualified form.
    assert_exit 2 15
    assert_stderr_contains 15 "repo-a"
    assert_stderr_contains 15 "repo-b"
    assert_stderr_contains 15 "<source>/<id>"

    # The qualified form resolves, then refuses for the missing identity row.
    assert_exit 2 16
    assert_stderr_contains 16 "aub task ingest"

    # So does the bare id that resolves to exactly one tracker.
    assert_exit 2 17
    assert_stderr_contains 17 "aub task ingest"

    # The per-command help carries the bead-id form.
    assert_exit 0 18
    assert_stdout_contains 18 "BEAD-ID"
    assert_stdout_contains 18 "--task-kind"
}
