# `aub calibrate begin|status|end` records a controlled experiment without a
# resident process: begin writes the premise against a real sampled baseline,
# the scheduler (`sample --account`) keeps sampling between the commands, and
# end records the end of controlled work without declaring the meter settled.
# Each command is its own process and its own step, so the experiment
# surviving across the steps proves no in-memory state was required.

CASE_ID="023-calibrate-controlled-experiment"
CASE_DESCRIPTION="aub calibrate begin, status and end run as separate processes against the synthetic server, with scheduler sampling between commands."

SERVER_PID=""
PORT=""

case_preconditions() {
    require_command "$AUB_BIN"
    require_command sqlite3
    require_command python3

    mkdir -p "$STATE_DIR/home" "$STATE_DIR/creds"
    echo '{"accessToken":"test-token"}' > "$STATE_DIR/creds/token.json"

    cat > "$STATE_DIR/aub.toml" <<CFG_EOF
state.dir = "$STATE_DIR"

[[accounts]]
name = "work-primary"
provider = "anthropic"
credential = { kind = "file", path = "$STATE_DIR/creds/token.json" }
CFG_EOF

    python3 -c "
import http.server
import socketserver
import sys

port_file = sys.argv[1]
fixture = sys.argv[2]

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
" "$STATE_DIR/port.txt" "$REPO_ROOT/tests/fixtures/meter/anthropic/idle-five-hour.json" &
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
    # 1. Seed the complete published cost model the experiment references.
    step "seed-complete-cost-model" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "$AUB_BIN" __cost-model-fixture complete

    # 2. Sample once against the stub server: the baseline observation.
    step "sample-baseline" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "AUB_ANTHROPIC_ENDPOINT=http://127.0.0.1:$PORT" \
        "$AUB_BIN" sample --account work-primary

    # 3. Begin the experiment in its own process, then exit.
    step "calibrate-begin" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "$AUB_BIN" calibrate --account work-primary begin \
        --plan-tier pro-5h --window five_hour \
        --cost-model anthropic-claude-messages-v1 \
        --experiment exp-controlled --assert-exclusive

    # 4. The scheduler keeps sampling while no calibrate process runs.
    step "sample-between-commands" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "AUB_ANTHROPIC_ENDPOINT=http://127.0.0.1:$PORT" \
        "$AUB_BIN" sample --account work-primary

    # 5. Status in its own process sees the running phase and both samples.
    step "calibrate-status-running" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "$AUB_BIN" calibrate status --experiment exp-controlled

    # 6. End the controlled work in its own process.
    step "calibrate-end" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "$AUB_BIN" calibrate end --experiment exp-controlled

    # 7. Status after end reports the ended phase, not a settlement verdict.
    step "calibrate-status-ended" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "$AUB_BIN" calibrate status --experiment exp-controlled
}

case_assertions() {
    if [ -n "${SERVER_PID:-}" ]; then
        kill "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
    fi

    # Step 1: the complete seed model is active.
    assert_exit 0 1
    assert_stdout_contains 1 "cost model anthropic-claude-messages-v1 active"

    # Step 2: the baseline sample succeeds.
    assert_exit 0 2
    assert_stdout_contains 2 "sample: account=work-primary outcome=success"

    # Step 3: begin records the premise and starts no resident process.
    assert_exit 0 3
    assert_stdout_contains 3 "calibrate begin: experiment=exp-controlled"
    assert_stdout_contains 3 "no resident process was started"

    # Step 4: sampling between the commands succeeds.
    assert_exit 0 4
    assert_stdout_contains 4 "sample: account=work-primary outcome=success"

    # Step 5: status reports the running phase and both samples.
    assert_exit 0 5
    assert_stdout_contains 5 "phase=running"
    assert_stdout_contains 5 "samples_since_baseline=2"

    # Step 6: end records the boundary without declaring settlement.
    assert_exit 0 6
    assert_stdout_contains 6 "calibrate end: experiment=exp-controlled"
    assert_stdout_contains 6 "not declared settled"

    # Step 7: status after end reports the ended phase.
    assert_exit 0 7
    assert_stdout_contains 7 "phase=ended"
}
