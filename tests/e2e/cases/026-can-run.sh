# aub-cab.4: `aub can-run` joins a fresh meter sample, per-window calibration
# health, the active cost model and the historical task distribution into one
# advisory, run against the release binary because the property under test is
# end-to-end: a real HTTP fetch, a real persisted sample, a real projection
# read, real calibration and cost-model rows, and real ingested task history,
# all through the shipped binary rather than a unit test's hand-built input.
#
# The synthetic provider server (`crates/test-support::SyntheticServer`) is a
# Rust-only fixture that never links into the release binary this runner
# exercises (`010-synthetic-server.sh` exists to prove exactly that), so no
# shell case anywhere in this suite drives it; every case that needs a live
# reading instead points `AUB_ANTHROPIC_ENDPOINT` at a small stub HTTP server
# (`022-sample-idle-five-hour.sh`'s own pattern) or at an unreachable port.
# This case follows the same convention: a stub server answers the worked
# example's window shape once, then is killed before the two scenarios that
# must prove they read no network at all.
#
# Two calibration/cost-model seams have no CLI path yet (`load_active_at`'s
# scope key requires a calibration row nothing here ingests, and the same is
# true of an active cost model), so this case seeds both through the real
# store functions behind two test-only hooks (`__cost-model-fixture`,
# `__calibration-fixture`) rather than hand-writing SQL against schemas this
# file does not own. Task identity has the same gap (no CLI resolves a task's
# kind yet) and is seeded directly against `task_identity`, matching this
# suite's own convention for every table with no ingestion path
# (`025-now-account-switch-boundary.sh` does the same for
# `session_account_marker`).
#
# What this case does not cover: a policy-classified missing-model-window case
# (`aub-eun.15`) needs a second sample whose response drops a previously
# present window, which needs a second stub-server fixture and a second
# forced sample layered onto the same account; left for a follow-up case
# rather than seeded thin here.

CASE_ID="026-can-run"
CASE_DESCRIPTION="aub can-run selects the limiting window by calibrated headroom rather than percentage, refuses an uncalibrated window without hiding the calibrated ones, and refuses a stale meter, all as a normal exit 0."

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

[[accounts]]
name = "stale-primary"
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
CFG_EOF

    # Three completed "task"-kind tasks, one session and one post-claim
    # usage record each, so the historical distribution has n=3 (the
    # configured minimum above) rather than refusing as insufficient.
    cat > "$STATE_DIR/transcripts/claude-code/session.jsonl" <<'JSONL'
{"type":"assistant","timestamp":"2026-08-25T01:00:00.000Z","sessionId":"s1","message":{"id":"m1","usage":{"input_tokens":1000,"output_tokens":500000,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}
{"type":"assistant","timestamp":"2026-08-25T03:00:00.000Z","sessionId":"s2","message":{"id":"m2","usage":{"input_tokens":1000,"output_tokens":800000,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}
{"type":"assistant","timestamp":"2026-08-25T05:00:00.000Z","sessionId":"s3","message":{"id":"m3","usage":{"input_tokens":1000,"output_tokens":1100000,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}
JSONL

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

    # A stub HTTP server answering the worked example's window shape: an
    # account-wide five-hour window at 38.0% remaining, an account-wide
    # seven-day window at 70.0% remaining, a sonnet-specific seven-day
    # window at 52.0% remaining (calibrated far more tightly, so it limits
    # despite the higher remaining percentage), and an opus-specific
    # seven-day window left uncalibrated on purpose.
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
    # 1-2. Land the three tasks' canonical usage and claim/release boundaries.
    step "ingest-transcripts" env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG" \
        "$AUB_BIN" ingest transcripts
    step "task-ingest" env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG" \
        "$AUB_BIN" task ingest

    # 3. Task identity: no CLI resolves a task's kind yet, so this is seeded
    #    directly, matching this suite's convention for every table with no
    #    ingestion path of its own.
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

    # 4. Explicit launcher-or-hook account markers for the three sessions, so
    #    their tasks read as attributed rather than pushing the corpus below
    #    the attribution-quality floor.
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

    # 5. An active, complete cost model: no CLI activates one outside this
    #    test-only hook (`aub-cab.4`'s own scope note applies here too).
    step "seed-cost-model" env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG" \
        "$AUB_BIN" __cost-model-fixture complete

    # 6-8. A current calibration for every window except the opus-specific
    #    one, seeded through the real experiment/result/activation chain.
    step "seed-calibration-five-hour" env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG" \
        "$AUB_BIN" __calibration-fixture five_hour 100
    step "seed-calibration-seven-day" env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG" \
        "$AUB_BIN" __calibration-fixture seven_day 100
    step "seed-calibration-seven-day-sonnet" env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG" \
        "$AUB_BIN" __calibration-fixture seven_day_sonnet 40

    # 9. The worked example: default (fresh) mode forces a real sample
    #    against the stub server first, then advises. seven_day_sonnet's
    #    tighter calibration (40 micros/point vs 100 for the other two) makes
    #    it the limiting window despite five_hour's lower remaining
    #    percentage (38.0% vs 52.0%): headroom, not percentage, decides.
    step "can-run-worked-example" env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG" \
        "AUB_ANTHROPIC_ENDPOINT=http://127.0.0.1:$PORT" \
        "$AUB_BIN" can-run --task-kind task --account work-primary --task-model sonnet

    # The stub server has answered what this case needs; killing it before
    # the next two steps is how they prove no live fetch happens.
    kill "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
    SERVER_PID=""

    # 10. --cached reads the persisted reading with the server dead: opus
    #    additionally constrains on seven_day_opus, which was never
    #    calibrated, so this refuses even though seven_day (an unrelated,
    #    fully calibrated account-wide window also constraining opus) stays
    #    present and current.
    step "can-run-uncalibrated-window" env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG" \
        "$AUB_BIN" can-run --task-kind task --account work-primary --task-model opus --cached

    # 11. A never-sampled account against an unreachable endpoint refuses as
    #    a stale meter, in the unknown form, still exit 0.
    step "can-run-stale-meter" env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG" \
        "AUB_ANTHROPIC_ENDPOINT=http://127.0.0.1:9" \
        "$AUB_BIN" can-run --task-kind task --account stale-primary --task-model sonnet

    # 12. The same worked example in JSON: the limiting window identity and
    #    both credit-interval endpoints travel in the envelope.
    step "can-run-worked-example-json" env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG" \
        "AUB_ANTHROPIC_ENDPOINT=http://127.0.0.1:9" \
        "$AUB_BIN" can-run --task-kind task --account work-primary --task-model sonnet --cached --format json
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

    # Step 9: the worked example. Rendered text carries "observed Ns ago"
    # (real elapsed time since the stub server answered), so this step is
    # asserted by substring rather than `assert_golden`'s byte-exact
    # comparison; the two fully deterministic refusal steps below are
    # golden-compared instead.
    assert_exit 0 9
    assert_stdout_contains 9 "can-run: task"
    assert_stdout_contains 9 "account: work-primary"
    assert_stdout_contains 9 "model: sonnet"
    assert_stdout_contains 9 "lowest remaining percentage: five_hour"
    assert_stdout_contains 9 "limiting calibrated window:  seven_day_sonnet"
    assert_stdout_contains 9 "headroom 20"
    assert_stdout_contains 9 "headroom 38"
    assert_stdout_contains 9 "headroom 70"
    assert_stdout_contains 9 "n = 3"
    assert_stdout_contains 9 "median = 12 credits"
    assert_stdout_contains 9 "p25–p75 = 7–16 credits"
    assert_stdout_contains 9 "assessment: MARGINAL"
    assert_stdout_contains 9 "limiting window: seven_day_sonnet"

    assert_exit 0 10
    assert_golden 10 "$REPO_ROOT/tests/e2e/golden-026-can-run-uncalibrated-window.txt"

    assert_exit 0 11
    assert_golden 11 "$REPO_ROOT/tests/e2e/golden-026-can-run-stale-meter.txt"

    assert_exit 0 12
    assert_json_field 12 command can-run
    assert_json_field 12 limiting_window seven_day_sonnet
    assert_json_field 12 outcome.status ready
    assert_json_field 12 outcome.assessment MARGINAL
    assert_json_field 12 outcome.windows[0].headroom.lower 20800000
    assert_json_field 12 outcome.windows[0].headroom.upper 20800000
}
