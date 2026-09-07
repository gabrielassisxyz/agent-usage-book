# `aub sample` against an endpoint returning an idle five-hour window persists
# the observation with `WindowResetState::NotStarted` (no persist-failed error),
# records both the not-started five-hour window and the active seven-day window,
# and updates the projection so `aub status` renders the account with no window
# in progress. Rebuilding the projection from the ledger reconstructs the state.
#
# A second account takes its credential from an environment variable
# (aub-e2uz): the variable resolves in `aub config`, one `aub now` against the
# stub provider succeeds with it, and the state directory retains no trace of
# the value the variable carried.

CASE_ID="022-sample-idle-five-hour"
CASE_DESCRIPTION="aub sample persists a not-started window without error, status renders no window in progress, an env credential resolves end to end, and state retains no credential material."

SERVER_PID=""
PORT=""
ENV_MATERIAL=""

case_preconditions() {
    require_command "$AUB_BIN"
    require_command sqlite3
    require_command python3

    # Distinctive on purpose: a one-character value like the credential the
    # kind was designed for cannot anchor a leak grep, since a stray letter
    # matches schema text in the ledger that carries no credential. This value
    # is what the grep step hunts for, so it must match nothing but a leak.
    ENV_MATERIAL="env-material-never-persist-4b1f"

    mkdir -p "$STATE_DIR/home" "$STATE_DIR/creds"
    echo '{"accessToken":"test-token"}' > "$STATE_DIR/creds/token.json"

    cat > "$STATE_DIR/aub.toml" <<CFG_EOF
state.dir = "$STATE_DIR"

[[accounts]]
name = "work-primary"
provider = "anthropic"
credential = { kind = "file", path = "$STATE_DIR/creds/token.json" }

[[accounts]]
name = "env-primary"
provider = "anthropic"
credential = { kind = "env", name = "AUB_TEST_TOKEN" }
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
    # 1. Sample account against stub server returning idle-five-hour.json
    step "sample-account-with-idle-window" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "AUB_ANTHROPIC_ENDPOINT=http://127.0.0.1:$PORT" \
        "$AUB_BIN" sample --account work-primary

    # 2. Query SQLite ledger to verify both five_hour (not_started) and seven_day (known) are stored
    step "query-meter-window" sqlite3 -separator ',' "$STATE_DIR/ledger.db" \
        "SELECT semantic_key, resets_at, reset_state, quota_used_ppm FROM meter_window ORDER BY semantic_key ASC"

    # 3. Status reads projection and reports no window in progress
    step "status" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "$AUB_BIN" status

    # 4. Remove projection to verify rebuild path
    step "remove-projection" rm -f "$STATE_DIR/projection"

    # 5. Doctor --fix rebuilds the projection from the ledger holding not-started windows
    step "doctor-rebuild-projection" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "$AUB_BIN" doctor --fix

    # 6. Status reads rebuilt projection and reports no window in progress
    step "status-after-rebuild" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "$AUB_BIN" status

    # 7. The env credential resolves in `aub config`: the table is accepted and
    #    the account is part of the resolved configuration.
    step "config-resolves-env-credential" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "AUB_TEST_TOKEN=$ENV_MATERIAL" \
        "$AUB_BIN" config

    # 8. One `aub now` for the env-credential account against the stub
    #    provider: the variable's value is the credential material the request
    #    goes out with, and the attempt succeeds.
    step "now-env-credential" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "AUB_ANTHROPIC_ENDPOINT=http://127.0.0.1:$PORT" \
        "AUB_TEST_TOKEN=$ENV_MATERIAL" \
        "$AUB_BIN" now --account env-primary

    # 9. The value the variable carried is nowhere in the state directory: not
    #    in the ledger, the spool, the projection, or any diagnostic the run
    #    retained. The grep exits 1 (leak) when it finds the value, and this
    #    step's assertion demands exit 0.
    step "state-has-no-credential-material" bash -c \
        'if grep -rqF -- "$1" "$2"; then echo "credential material found in state"; exit 1; fi' \
        _ "$ENV_MATERIAL" "$STATE_DIR"
}

case_assertions() {
    if [ -n "${SERVER_PID:-}" ]; then
        kill "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
    fi

    # Step 1: sample succeeds without persist-failed
    assert_exit 0 1
    assert_stdout_contains 1 "sample: account=work-primary outcome=success"

    # Step 2: ledger holds both windows with correct reset_state
    assert_exit 0 2
    assert_stdout_contains 2 "five_hour,,not_started,0"
    assert_stdout_contains 2 "seven_day,"
    assert_stdout_contains 2 ",known,0"

    # Step 3: status shows no window in progress
    assert_exit 0 3
    assert_stdout_contains 3 "work-primary  anthropic"
    assert_stdout_contains 3 "not started"

    # Step 4: rm -f exits 0
    assert_exit 0 4

    # Step 5: doctor --fix successfully rebuilds projection
    assert_exit 0 5
    assert_stdout_contains 5 "republished at generation"

    # Step 6: status after rebuild still shows no window in progress
    assert_exit 0 6
    assert_stdout_contains 6 "work-primary  anthropic"
    assert_stdout_contains 6 "not started"

    # Step 7: the env credential table resolved without error
    assert_exit 0 7
    assert_stdout_contains 7 "accounts"

    # Step 8: now with the env credential succeeded against the stub provider
    assert_exit 0 8
    assert_stdout_contains 8 "aub env-primary"

    # Step 9: the variable's value left no trace in the state directory
    assert_exit 0 9
}
