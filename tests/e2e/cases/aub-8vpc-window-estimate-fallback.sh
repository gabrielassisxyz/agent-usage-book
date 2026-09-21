# aub-8vpc: a percent-of-window rate card stands in for a missing calibration,
# labelled estimated on every surface, and loses to a current calibration.
#
# One ledger walks the precedence rule in order, through the shipped binary:
# neither a calibration nor an estimate card (both commands refuse naming the
# calibration), an estimate card and no calibration (both answer, labelled
# `(estimated)`, and doctor reports the estimate in use), then a current
# calibration beside the same card (both answer from the calibration, with no
# label, and doctor's line is gone).
#
# The fourth row of the table, a calibration that is recorded but not
# current, has no CLI path to produce: no production caller supplies the drift
# or review-due facts that make a stored calibration `review_due` or
# `suspect`. Those rows are pinned where the decision is made, by
# `spend_window_precedence_tests::the_spend_precedence_table` in `src/cli.rs`
# and `a_review_due_calibration_refuses_instead_of_falling_back` in
# `src/report/can_run.rs`.
#
# The meter comes from a stub HTTP server answering the can-run worked
# example's window shape once (`026-can-run.sh`'s pattern); every later can-run
# reads that persisted sample with `--cached`, so the server is killed right
# after the first one.

CASE_ID="aub-8vpc-window-estimate-fallback"
CASE_DESCRIPTION="aub spend --window-equivalent and aub can-run fall back to a labelled rate-card estimate only when no calibration is recorded, and a current calibration outranks it."

CONFIG=""
LEDGER_DB=""
SERVER_PID=""
PORT=""

case_preconditions() {
    require_command "$AUB_BIN"
    require_command sqlite3
    require_command python3

    LEDGER_DB="$STATE_DIR/ledger.db"
    CONFIG="$STATE_DIR/aub.toml"

    mkdir -p "$STATE_DIR/home" "$STATE_DIR/creds" \
        "$STATE_DIR/transcripts/claude-code" "$STATE_DIR/tracker"
    echo '{"accessToken":"test-token"}' > "$STATE_DIR/creds/token.json"

    cat > "$CONFIG" <<CFG_EOF
state.dir = "$STATE_DIR"

[[accounts]]
name = "work-primary"
provider = "anthropic"
credential = { kind = "file", path = "$STATE_DIR/creds/token.json" }

[task_distribution]
min_samples = 3

[[transcripts]]
name = "claude-code"
root = "$STATE_DIR/transcripts/claude-code"
pattern = "**/*.jsonl"
format = "claude-code"

[tracker]
kind = "local"
path = "$STATE_DIR/tracker"

[[models]]
pattern = "sonnet"
vendor = "anthropic"
model = "claude-sonnet-4"
CFG_EOF

    # 3,000 input and 2,400,000 output tokens in all. At 1.0 and 0.5 points
    # per million the estimate is 0.0030 + 1.2000 = 1.2030 points exactly.
    cat > "$STATE_DIR/transcripts/claude-code/session.jsonl" <<'JSONL'
{"type":"assistant","timestamp":"2026-08-25T01:00:00.000Z","sessionId":"s1","message":{"id":"m1","model":"claude-sonnet-4","usage":{"input_tokens":1000,"output_tokens":500000,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}
{"type":"assistant","timestamp":"2026-08-25T03:00:00.000Z","sessionId":"s2","message":{"id":"m2","model":"claude-sonnet-4","usage":{"input_tokens":1000,"output_tokens":800000,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}
{"type":"assistant","timestamp":"2026-08-25T05:00:00.000Z","sessionId":"s3","message":{"id":"m3","model":"claude-sonnet-4","usage":{"input_tokens":1000,"output_tokens":1100000,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}
JSONL

    cat > "$STATE_DIR/estimates.toml" <<'TOML'
[[card]]
vendor = "anthropic"
model = "claude-sonnet-4"
token_class = "input"
billing_basis = "percent_of_window_per_million_tokens"
window = "five_hour"
rate = "1.0"
unit = "percentage_points"
quality = "estimate"
source = "e2e fixture approximation"
effective_start = "2026-01-01"

[[card]]
vendor = "anthropic"
model = "claude-sonnet-4"
token_class = "output"
billing_basis = "percent_of_window_per_million_tokens"
window = "five_hour"
rate = "0.5"
unit = "percentage_points"
quality = "estimate"
source = "e2e fixture approximation"
effective_start = "2026-01-01"
TOML

    sqlite3 "$STATE_DIR/tracker/beads.db" <<'SQL'
CREATE TABLE events (
    id INTEGER PRIMARY KEY,
    issue_id TEXT NOT NULL,
    event_type TEXT NOT NULL,
    actor TEXT,
    old_value TEXT,
    new_value TEXT,
    created_at TEXT NOT NULL
);
INSERT INTO events (id, issue_id, event_type, actor, old_value, new_value, created_at) VALUES
 (1, 'aub-1', 'status_changed', 'agent-1', 'open', 'in_progress', '2026-08-25T00:30:00Z'),
 (2, 'aub-1', 'status_changed', 'agent-1', 'in_progress', 'closed', '2026-08-25T02:00:00Z'),
 (3, 'aub-2', 'status_changed', 'agent-1', 'open', 'in_progress', '2026-08-25T02:30:00Z'),
 (4, 'aub-2', 'status_changed', 'agent-1', 'in_progress', 'closed', '2026-08-25T04:00:00Z'),
 (5, 'aub-3', 'status_changed', 'agent-1', 'open', 'in_progress', '2026-08-25T04:30:00Z'),
 (6, 'aub-3', 'status_changed', 'agent-1', 'in_progress', 'closed', '2026-08-25T06:00:00Z');
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

aub_step() {
    local name="$1"
    shift
    step "$name" env "HOME=$STATE_DIR/home" "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$CONFIG" "AUB_ANTHROPIC_ENDPOINT=http://127.0.0.1:${PORT:-9}" \
        "$AUB_BIN" "$@"
}

# A step that passes only when TEXT is absent from an earlier step's stdout:
# the label and the doctor line have to be shown missing, not just unasserted.
absent_step() {
    local name="$1" text="$2" from="$3"
    step "$name" sh -c '! grep -qF -- "$1" "$2"' _ "$text" "$(step_dir "$from")/stdout.bin"
}

case_steps() {
    # 1-4. Usage, task boundaries, task identity and account attribution,
    #      seeded exactly as 026-can-run.sh does.
    aub_step "ingest-transcripts" ingest transcripts
    aub_step "task-ingest" task ingest
    step "seed-task-identity" sqlite3 "$LEDGER_DB" "
        INSERT INTO task_identity (
            task_source, task_native, state, kind, winner_origin, evidence,
            normalization_version, size_state, size, size_evidence,
            difficulty_state, difficulty, difficulty_evidence
        ) VALUES
         ('beads', 'aub-1', 'resolved', 'task', 'tracker_field:kind', '{}', 1, 'unknown', NULL, '{}', 'unknown', NULL, '{}'),
         ('beads', 'aub-2', 'resolved', 'task', 'tracker_field:kind', '{}', 1, 'unknown', NULL, '{}', 'unknown', NULL, '{}'),
         ('beads', 'aub-3', 'resolved', 'task', 'tracker_field:kind', '{}', 1, 'unknown', NULL, '{}', 'unknown', NULL, '{}');
    "
    step "seed-account-markers" sqlite3 "$LEDGER_DB" "
        INSERT INTO session_account_marker
            (session_source, session_native, observed_at, source_ordering_key,
             logical_account, resolved_account_id, marker_source, run_source,
             run_native, evidence_designation)
        VALUES
         ('claude-code', 's1', 1787617800000000000, NULL, 'work-primary', NULL, 'hook', NULL, NULL, 'launcher_or_hook'),
         ('claude-code', 's2', 1787625000000000000, NULL, 'work-primary', NULL, 'hook', NULL, NULL, 'launcher_or_hook'),
         ('claude-code', 's3', 1787632200000000000, NULL, 'work-primary', NULL, 'hook', NULL, NULL, 'launcher_or_hook');
    "

    # 5-7. A cost model and a calibration for every window except five_hour,
    #      the one this case is about.
    aub_step "seed-cost-model" cost-model activate anthropic_claude_messages_v1
    aub_step "seed-calibration-seven-day" __calibration-fixture seven_day 100
    aub_step "seed-calibration-seven-day-sonnet" __calibration-fixture seven_day_sonnet 40

    # 8-10. Neither a calibration nor an estimate card for five_hour.
    aub_step "spend-neither" spend --since 2026-08-25 --days 1 --group-by account \
        --window-equivalent five_hour --refresh never
    aub_step "can-run-neither" can-run --task-kind task --account work-primary --task-model sonnet
    aub_step "can-run-neither-json" can-run --task-kind task --account work-primary \
        --task-model sonnet --cached --format json

    kill "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
    SERVER_PID=""
    PORT=""

    # 11-17. The estimate cards, and still no five_hour calibration.
    aub_step "import-estimates" rate-card import "$STATE_DIR/estimates.toml"
    aub_step "spend-estimate" spend --since 2026-08-25 --days 1 --group-by account \
        --window-equivalent five_hour --refresh never
    aub_step "spend-estimate-json" spend --since 2026-08-25 --days 1 --group-by account \
        --window-equivalent five_hour --refresh never --format json
    aub_step "can-run-estimate" can-run --task-kind task --account work-primary \
        --task-model sonnet --cached
    aub_step "can-run-estimate-json" can-run --task-kind task --account work-primary \
        --task-model sonnet --cached --format json
    aub_step "doctor-estimate" doctor
    aub_step "rate-card-reimport" rate-card import "$STATE_DIR/estimates.toml"

    # 18-26. A current five_hour calibration beside the same cards.
    aub_step "seed-calibration-five-hour" __calibration-fixture five_hour 100
    aub_step "spend-calibrated" spend --since 2026-08-25 --days 1 --group-by account \
        --window-equivalent five_hour --refresh never
    absent_step "spend-calibrated-unlabelled" "(estimated)" 19
    aub_step "can-run-calibrated" can-run --task-kind task --account work-primary \
        --task-model sonnet --cached
    absent_step "can-run-calibrated-unlabelled" "(estimated)" 21
    aub_step "doctor-calibrated" doctor
    absent_step "doctor-calibrated-silent" "anthropic/five_hour" 23
    aub_step "can-run-calibrated-json" can-run --task-kind task --account work-primary \
        --task-model sonnet --cached --format json
    absent_step "can-run-calibrated-json-has-no-basis" "rate_card_estimate" 25
}

case_assertions() {
    if [ -n "${SERVER_PID:-}" ]; then
        kill "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
    fi

    local n
    for n in 1 2 3 4 5 6 7; do
        assert_exit 0 "$n"
    done

    # Neither a calibration nor a card: both refuse naming the calibration.
    assert_exit 0 8
    assert_row_detail_contains 8 "^│  work-primary " \
        "window equivalent unavailable: active calibration for provider anthropic and window five_hour"
    assert_exit 0 9
    assert_stdout_contains 9 "no calibration is recorded for this window"
    assert_exit 0 10
    assert_json_field 10 "outcome.status" "refused"
    assert_stdout_contains 10 '"subject":"five_hour","reason":"no calibration is recorded for this window"'

    # The cards and no calibration: both answer, labelled, and doctor says so.
    assert_exit 0 11
    assert_stdout_contains 11 "added=2"
    assert_exit 0 12
    assert_row_detail_contains 12 "^│  work-primary " \
        "window equivalent [1.2030, 1.2030] percentage points (estimated) from rate cards 1, 2"
    assert_exit 0 13
    assert_json_field 13 "groups[0].window_equivalent.evidence_quality" "estimated"
    assert_json_field 13 "groups[0].window_equivalent.methods[0]" "rate-card-estimate"
    assert_json_field 13 "groups[0].window_equivalent.basis.rate_card_ids[0]" "1"
    assert_json_field 13 "groups[0].window_equivalent.basis.rate_card_ids[1]" "2"
    assert_json_field 13 "groups[0].window_equivalent.lower" "12030"
    assert_exit 0 14
    assert_stdout_matches 14 "five_hour .*headroom .* credits \\(estimated\\)$"
    assert_stdout_matches 14 "seven_day .*headroom .* credits$"
    assert_exit 0 15
    assert_stdout_contains 15 '"basis":{"kind":"rate_card_estimate","rate_card_ids":[1,2]},"evidence_quality":"estimated"'
    assert_stdout_contains 16 "[INFO] window-estimate-in-use: window figures come from a rate-card estimate for: anthropic/five_hour"
    assert_exit 0 17
    assert_stdout_contains 17 "added=0 unchanged=2"

    # A current calibration beside the same cards: it answers, unlabelled.
    assert_exit 0 18
    assert_exit 0 19
    assert_row_detail_contains 19 "^│  work-primary " "calibration five_hour-fixture-calibration"
    assert_exit 0 20
    assert_exit 0 21
    assert_stdout_matches 21 "five_hour .*calibration #five_hour-fixture-calibration.*headroom .* credits$"
    assert_exit 0 22
    assert_stdout_contains 23 "[PASS] window-estimate-in-use"
    assert_exit 0 24
    assert_exit 0 25
    assert_stdout_contains 25 '"semantic_key":"five_hour"'
    assert_exit 0 26
}
