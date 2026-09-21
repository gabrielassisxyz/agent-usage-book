# aub-8pjw: a stored window calibration whose review instant has passed makes
# `aub spend --window-equivalent` and `aub can-run` refuse, naming its health
# and exiting non-zero, even with a percent-of-window estimate card in force.
#
# One ledger walks a five_hour calibration from current to review-due through
# the shipped binary. The review instant is the calibration's fit time plus
# `calibration.review_after` (`aub-6omr`). First the default horizon of 30
# days applies: both commands answer from the calibration, unlabelled, and the
# estimate cards stay inert (the planted negative). Then the case waits past
# a one-second horizon, set through `AUB_CALIBRATION_REVIEW_AFTER`, and the
# same commands over the same ledger refuse with `review_due`, exit 6, and
# print no estimate figure: a measurement asking for review is not papered
# over by an approximation. The JSON form of the refused spend carries the
# same refusal and no figure of any kind (`aub-ov2f`).
#
# The meter comes from a stub HTTP server answering the can-run worked
# example's window shape once (`aub-8vpc-window-estimate-fallback.sh`'s
# pattern); every later can-run reads that persisted sample with `--cached`.

CASE_ID="aub-8pjw-calibration-review-due-refuses"
CASE_DESCRIPTION="aub spend --window-equivalent and aub can-run refuse, naming review_due and exiting non-zero, once a stored calibration passes its configured review instant, and answer from it before."

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
    # The two cache cards price no token here but complete the cost model's
    # classes, without which can-run refuses the five-hour estimate.
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

[[card]]
vendor = "anthropic"
model = "claude-sonnet-4"
token_class = "cache_read"
billing_basis = "percent_of_window_per_million_tokens"
window = "five_hour"
rate = "0.1"
unit = "percentage_points"
quality = "estimate"
source = "e2e fixture approximation"
effective_start = "2026-01-01"

[[card]]
vendor = "anthropic"
model = "claude-sonnet-4"
token_class = "cache_write_5m"
billing_basis = "percent_of_window_per_million_tokens"
window = "five_hour"
rate = "1.25"
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

# The review horizon every aub step runs under: the configured default until
# the case walks the calibration past its review instant.
REVIEW_AFTER="30d"

aub_step() {
    local name="$1"
    shift
    step "$name" env "HOME=$STATE_DIR/home" "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$CONFIG" "AUB_ANTHROPIC_ENDPOINT=http://127.0.0.1:${PORT:-9}" \
        "AUB_CALIBRATION_REVIEW_AFTER=$REVIEW_AFTER" \
        "$AUB_BIN" "$@"
}

# Succeeds only when TEXT is absent from FILE read as one line: box rails
# stripped and every line joined by one space, as assert_row_detail_contains
# reads a detail, so a figure the table wrapped across two lines is still
# found (`aub-ov2f`).
text_absent_from_joined_lines() {
    local text="$1" file="$2" joined
    joined="$(awk '
        {
            line = $0
            sub(/^(│| )+/, "", line)
            sub(/( |│)+$/, "", line)
            joined = joined (joined == "" ? "" : " ") line
        }
        END { print joined }
    ' "$file")"
    [[ "$joined" != *"$text"* ]]
}

# A step that passes only when TEXT is absent from an earlier step's stdout:
# the estimate label and figure have to be shown missing, not just unasserted.
absent_step() {
    local name="$1" text="$2" from="$3"
    step "$name" text_absent_from_joined_lines "$text" "$(step_dir "$from")/stdout.bin"
}

# Succeeds only when the spend report, the first JSON document on FILE's
# stdout (the error envelope follows it), refuses every group's window
# equivalent for review and carries no figure: no interval endpoint, no
# calibration id and no evidence quality anywhere in it.
spend_json_withholds_review_due_figures() {
    jq -se '
        .[0].groups as $groups
        | ($groups | length) > 0
        and all($groups[];
            .window_equivalent
            | .status == "unavailable"
            and any(.missing[]; endswith("calibration health is review_due"))
            and (has("lower") or has("upper") or has("calibration_id")
                 or has("evidence_quality") | not))
    ' "$1" >/dev/null
}

case_steps() {
    # 1-4. Usage, task boundaries, task identity and account attribution,
    #      seeded exactly as aub-8vpc-window-estimate-fallback.sh does.
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

    # 5-9. A cost model, a calibration for every window, and the estimate
    #      cards for five_hour, which must stay inert while it is current.
    aub_step "seed-cost-model" cost-model activate anthropic_claude_messages_v1
    aub_step "seed-calibration-seven-day" __calibration-fixture seven_day 100
    aub_step "seed-calibration-seven-day-sonnet" __calibration-fixture seven_day_sonnet 40
    aub_step "seed-calibration-five-hour" __calibration-fixture five_hour 100
    aub_step "import-estimates" rate-card import "$STATE_DIR/estimates.toml"

    # 10-13. Before the review instant (the default 30-day horizon): both
    #        commands answer from the calibration, with no estimate label.
    aub_step "spend-current" spend --since 2026-08-25 --days 1 --group-by account \
        --window-equivalent five_hour --refresh never
    absent_step "spend-current-not-from-cards" "from rate cards" 10
    aub_step "can-run-current" can-run --task-kind task --account work-primary --task-model sonnet
    absent_step "can-run-current-unlabelled" "(estimated)" 12

    kill "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
    SERVER_PID=""
    PORT=""

    # 14. Past the review instant: every calibration above was fitted at
    #     least a second before the commands below run, under a one-second
    #     horizon.
    step "wait-past-review-instant" sleep 1

    # 15-21. The same ledger, now review-due: both refuse, naming the health,
    #        exit non-zero, and print no estimate figure.
    REVIEW_AFTER="1s"
    aub_step "spend-review-due" spend --since 2026-08-25 --days 1 --group-by account \
        --window-equivalent five_hour --refresh never
    absent_step "spend-review-due-not-from-cards" "from rate cards" 15
    absent_step "spend-review-due-no-figure" "window equivalent [" 15
    aub_step "can-run-review-due" can-run --task-kind task --account work-primary \
        --task-model sonnet --cached
    absent_step "can-run-review-due-no-estimate" "(estimated)" 18
    aub_step "can-run-review-due-json" can-run --task-kind task --account work-primary \
        --task-model sonnet --cached --format json
    absent_step "can-run-review-due-json-has-no-basis" "rate_card_estimate" 20

    # 22-23. spend's JSON form refuses the same window and prints no figure.
    aub_step "spend-review-due-json" spend --since 2026-08-25 --days 1 --group-by account \
        --window-equivalent five_hour --refresh never --format json
    step "spend-review-due-json-withholds-figures" spend_json_withholds_review_due_figures \
        "$(step_dir 22)/stdout.bin"
}

case_assertions() {
    if [ -n "${SERVER_PID:-}" ]; then
        kill "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
    fi

    local n
    for n in 1 2 3 4 5 6 7 8 9; do
        assert_exit 0 "$n"
    done
    assert_stdout_contains 9 "added=4"

    # Current: the calibration answers and the cards are inert.
    assert_exit 0 10
    assert_row_detail_contains 10 "^│  work-primary " "calibration five_hour-fixture-calibration"
    assert_exit 0 11
    assert_exit 0 12
    assert_stdout_matches 12 "five_hour .*calibration #five_hour-fixture-calibration.*headroom .* credits$"
    assert_exit 0 13

    # Review-due: both refuse naming the health, and exit 6.
    assert_exit 0 14
    assert_exit 6 15
    assert_row_detail_contains 15 "^│  work-primary " \
        "window equivalent unavailable: current calibration for provider anthropic and window five_hour: calibration health is review_due"
    assert_exit 0 16
    assert_exit 0 17
    assert_exit 6 18
    assert_stdout_contains 18 "review_due"
    assert_exit 0 19
    assert_exit 6 20
    assert_json_field 20 "outcome.status" "refused"
    assert_stdout_contains 20 '"subject":"five_hour"'
    assert_exit 0 21
    assert_exit 6 22
    assert_stdout_contains 22 "calibration health is review_due"
    assert_exit 0 23
}
