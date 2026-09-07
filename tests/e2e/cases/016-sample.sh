# `aub sample` observes provider endpoints for due or selected accounts,
# recording session markers and evidence with durable attempt tracking.

CASE_ID="016-sample"
CASE_DESCRIPTION="aub sample records markers and attempt evidence with appropriate exit status across scheduled and require-success modes."

LEDGER_DB=""
OPENCODE_PORT=""
OPENCODE_SERVER_PID=""
OPENCODE_COOKIE_MATERIAL=""

case_preconditions() {
    require_command "$AUB_BIN"
    require_command sqlite3
    require_command python3

    LEDGER_DB="$STATE_DIR/ledger.db"

    mkdir -p "$STATE_DIR/home" "$STATE_DIR/creds"
    echo '{"accessToken":"test-token"}' > "$STATE_DIR/creds/token.json"

    # The opencode account's session-cookie material: distinctive on purpose,
    # so the state-directory leak grep matches nothing but a leak, and free
    # of every shared forbidden pattern. This is the bare cookie value the
    # operator exports; the adapter builds the `auth=<value>` header itself.
    OPENCODE_COOKIE_MATERIAL="fixture-session-cookie-9f2c-not-a-real-value"

    cat > "$STATE_DIR/aub.toml" <<CFG_EOF
state.dir = "$STATE_DIR"

[[accounts]]
name = "work-primary"
provider = "anthropic"
credential = { kind = "file", path = "$STATE_DIR/creds/token.json" }

[[accounts]]
name = "go-primary"
provider = "opencode"
credential = { kind = "env", name = "OPENCODE_SESSION_COOKIE" }
opencode_workspace = "wrk_2345ABCDEFGHJKLMNOPQRSTuvwx"
CFG_EOF

    # The synthetic opencode transport: one local server answering every
    # request with the committed valid.html fixture, so the sample workflow
    # exercises the whole adapter path without the real provider.
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
        self.send_header('Content-Type', 'text/html; charset=utf-8')
        self.send_header('Content-Length', str(len(data)))
        self.end_headers()
        self.wfile.write(data)
    def log_message(self, format, *args):
        pass

httpd = socketserver.TCPServer(('127.0.0.1', 0), Handler)
with open(port_file, 'w') as pf:
    pf.write(str(httpd.server_address[1]))
httpd.serve_forever()
" "$STATE_DIR/opencode-port.txt" "$REPO_ROOT/tests/fixtures/meter/opencode/valid.html" &
    OPENCODE_SERVER_PID=$!

    local count=0
    while [ ! -s "$STATE_DIR/opencode-port.txt" ]; do
        sleep 0.05
        count=$((count + 1))
        if [ "$count" -gt 60 ]; then
            echo "timed out waiting for the opencode stub server" >&2
            exit 1
        fi
    done
    OPENCODE_PORT=$(cat "$STATE_DIR/opencode-port.txt")
}

case_steps() {
    # 1. Unreachable endpoint in scheduled timer mode: records attempt and unreachable result, exits 0
    step "sample-due-unreachable-timer-exits-zero" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "AUB_ANTHROPIC_ENDPOINT=http://127.0.0.1:9" \
        "AUB_OPENCODE_ENDPOINT=http://127.0.0.1:$OPENCODE_PORT" \
        "OPENCODE_SESSION_COOKIE=$OPENCODE_COOKIE_MATERIAL" \
        "$AUB_BIN" sample --due

    # 2. Assert SQLite has attempt and unreachable result
    step "query-unreachable-evidence" sqlite3 "$LEDGER_DB" "SELECT count(*), outcome FROM meter_attempt_result GROUP BY outcome"

    # 3. Require-success with forced account against unreachable endpoint: records evidence and exits 4 (RemoteUnavailable)
    step "sample-account-require-success-exits-four" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "AUB_ANTHROPIC_ENDPOINT=http://127.0.0.1:9" \
        "$AUB_BIN" sample --account work-primary --require-success

    # 4. Marker recording with --if-due when not due: records marker to SQLite and skips network
    step "sample-if-due-with-marker" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "$AUB_BIN" sample --account work-primary --if-due --session-id "cli:test-sess-1"

    # 5. Assert marker exists in SQLite
    step "query-marker" sqlite3 "$LEDGER_DB" "SELECT session_native, logical_account FROM session_account_marker WHERE session_native = 'test-sess-1'"

    # 6. Bare aub sample with no selector samples every configured account
    step "sample-bare-no-selector" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "AUB_ANTHROPIC_ENDPOINT=http://127.0.0.1:9" \
        "AUB_OPENCODE_ENDPOINT=http://127.0.0.1:$OPENCODE_PORT" \
        "OPENCODE_SESSION_COOKIE=$OPENCODE_COOKIE_MATERIAL" \
        "$AUB_BIN" sample

    # 7. Refusal: aub sample --all exits 2 naming the bare form
    step "sample-all-refused" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "$AUB_BIN" sample --all

    # 8. The opencode account samples its workspace page through the session
    #    cookie, against the synthetic transport serving valid.html.
    step "sample-opencode-account" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "AUB_OPENCODE_ENDPOINT=http://127.0.0.1:$OPENCODE_PORT" \
        "OPENCODE_SESSION_COOKIE=$OPENCODE_COOKIE_MATERIAL" \
        "$AUB_BIN" sample --account go-primary

    # 9. One observation, three meter windows: rolling, weekly, monthly.
    step "query-opencode-meter-windows" sqlite3 -separator ',' "$LEDGER_DB" \
        "SELECT semantic_key, quota_used_ppm, reset_state FROM meter_window WHERE semantic_key IN ('rolling','weekly','monthly') ORDER BY semantic_key ASC"

    # 10. Status prints the go-primary block from the sampled windows.
    step "status-opencode-account" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "$AUB_BIN" status

    # 11. The session cookie left no trace in the state directory.
    step "opencode-cookie-leak-grep" sh -c \
        'if grep -rqF -- "$1" "$2"; then echo "cookie material found in state"; exit 1; fi' \
        _ "$OPENCODE_COOKIE_MATERIAL" "$STATE_DIR"
}

case_assertions() {
    if [ -n "${OPENCODE_SERVER_PID:-}" ]; then
        kill "$OPENCODE_SERVER_PID" 2>/dev/null || true
        wait "$OPENCODE_SERVER_PID" 2>/dev/null || true
    fi

    # Step 1: scheduled --due exits 0 even on transport failure
    assert_exit 0 1
    assert_stdout_contains 1 "sample: account=work-primary outcome=unreachable"

    # Step 2: attempt result recorded
    assert_exit 0 2
    assert_stdout_contains 2 "1|unreachable"

    # Step 3: --require-success exits 4 on unreachable
    assert_exit 4 3
    assert_stdout_contains 3 "sample: account=work-primary outcome=unreachable"

    # Step 4: --if-due records marker and skips sampling (account is not due)
    assert_exit 0 4
    assert_stdout_contains 4 "sample: account=work-primary not-due"

    # Step 5: marker is in SQLite
    assert_exit 0 5
    assert_stdout_contains 5 "test-sess-1|work-primary"

    # Step 6: bare sample with no selector samples every configured account
    assert_exit 0 6
    assert_stdout_contains 6 "sample: account=work-primary outcome=unreachable"

    # Step 7: aub sample --all exits 2 naming the bare form
    assert_exit 2 7
    assert_stderr_contains 7 "unknown argument: --all"
    assert_stderr_contains 7 "run aub sample alone"

    # Step 8: the opencode account samples successfully against the fixture
    assert_exit 0 8
    assert_stdout_contains 8 "sample: account=go-primary outcome=success"

    # Step 9: the three windows persist at the fixture's decimal percents
    assert_exit 0 9
    assert_stdout_contains 9 "monthly,648000,known"
    assert_stdout_contains 9 "rolling,0,known"
    assert_stdout_contains 9 "weekly,355000,known"

    # Step 10: status prints the go-primary block; the limiting window is
    # the monthly one (35.2% left, 30d)
    assert_exit 0 10
    assert_stdout_contains 10 "aub go-primary 35.2% left"

    # Step 11: the cookie material never persisted
    assert_exit 0 11
}
