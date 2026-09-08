# A failed sampling attempt stores what the provider said (aub-rfot): the
# sanitized error classification and message land in the attempt result's
# `sanitized_error_classification` column, `aub coverage`'s findings name each
# classification with its count, and `aub doctor` lists the classifications
# seen per account in the window. Two synthetic endpoints stand in for the
# provider: one answering every request with a 429 whose body carries the
# provider's own error type, one answering with a 401 shaped the same way, so
# the run exercises both failure paths of the codex endpoint adapter end to
# end without any real network call.

CASE_ID="033-sample-error-classification"
CASE_DESCRIPTION="aub sample records the provider's error classification and sanitized message on failed attempts, and coverage and doctor name them."

LEDGER_DB=""
ENDPOINT_429_PID=""
ENDPOINT_401_PID=""
PORT_429=""
PORT_401=""

case_preconditions() {
    require_command "$AUB_BIN"
    require_command sqlite3
    require_command python3

    LEDGER_DB="$STATE_DIR/ledger.db"

    # Two codex homes whose sessions entries are symlinks to one shared tree,
    # the production shape that forces the endpoint path (aub-er47). The
    # shared tree holds nothing: neither account may read a rollout.
    mkdir -p "$STATE_DIR/shared-sessions" \
        "$STATE_DIR/home-429" \
        "$STATE_DIR/home-401"
    ln -s "$STATE_DIR/shared-sessions" "$STATE_DIR/home-429/sessions"
    ln -s "$STATE_DIR/shared-sessions" "$STATE_DIR/home-401/sessions"
    cp "$REPO_ROOT/tests/fixtures/meter/codex/auth-fixture.json" \
        "$STATE_DIR/home-429/auth.json"
    cp "$REPO_ROOT/tests/fixtures/meter/codex/auth-fixture.json" \
        "$STATE_DIR/home-401/auth.json"

    cat > "$STATE_DIR/aub.toml" <<CFG_EOF
state.dir = "$STATE_DIR"

[[accounts]]
name = "codex-throttled"
provider = "codex"
credential = { kind = "file", path = "$STATE_DIR/home-429/auth.json" }
codex_home = "$STATE_DIR/home-429"

[[accounts]]
name = "codex-rejected"
provider = "codex"
credential = { kind = "file", path = "$STATE_DIR/home-401/auth.json" }
codex_home = "$STATE_DIR/home-401"
CFG_EOF

    # The two failure endpoints: fixed status, fixed sanitized error body.
    # The bodies carry the provider's own error.type spelling, the shape the
    # adapters read the classification from.
    start_endpoint() {
        local status="$1" body_file="$2" port_file="$3"
        python3 -c "
import http.server
import socketserver
import sys

port_file = sys.argv[1]
status = int(sys.argv[2])
body_file = sys.argv[3]

class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        with open(body_file, 'rb') as f:
            data = f.read()
        self.send_response(status)
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
" "$port_file" "$status" "$body_file" &
    }

    cat > "$STATE_DIR/error-429.json" <<'BODY'
{"error": {"type": "rate_limit_error", "message": "Rate limit exceeded. Please retry later."}}
BODY
    cat > "$STATE_DIR/error-401.json" <<'BODY'
{"error": {"type": "authentication_error", "message": "Invalid authentication token provided."}}
BODY

    start_endpoint 429 "$STATE_DIR/error-429.json" "$STATE_DIR/port-429.txt"
    ENDPOINT_429_PID=$!
    start_endpoint 401 "$STATE_DIR/error-401.json" "$STATE_DIR/port-401.txt"
    ENDPOINT_401_PID=$!

    local count=0
    while [ ! -s "$STATE_DIR/port-429.txt" ] || [ ! -s "$STATE_DIR/port-401.txt" ]; do
        sleep 0.05
        count=$((count + 1))
        if [ "$count" -gt 60 ]; then
            echo "timed out waiting for the failure endpoint stubs" >&2
            exit 1
        fi
    done
    PORT_429=$(cat "$STATE_DIR/port-429.txt")
    PORT_401=$(cat "$STATE_DIR/port-401.txt")
}

case_steps() {
    # 1. The throttled account samples against the 429 endpoint: unreachable,
    #    still exit zero on a forced sample.
    step "sample-throttled" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "AUB_CODEX_ENDPOINT=http://127.0.0.1:$PORT_429" \
        "$AUB_BIN" sample --account codex-throttled

    # 2. The failed attempt's result row names the provider's own
    #    classification with the sanitized message beside it.
    step "query-throttled-classification" sqlite3 "$LEDGER_DB" \
        "SELECT outcome, sanitized_error_classification FROM meter_attempt_result WHERE outcome <> 'success'"

    # 3. The rejected account samples against the 401 endpoint.
    step "sample-rejected" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "AUB_CODEX_ENDPOINT=http://127.0.0.1:$PORT_401" \
        "$AUB_BIN" sample --account codex-rejected

    # 4. That row names the authentication classification the same way.
    step "query-rejected-classification" sqlite3 "$LEDGER_DB" \
        "SELECT outcome, sanitized_error_classification FROM meter_attempt_result WHERE outcome = 'auth_required'"

    # 5. Coverage names each classification with its count in the findings.
    step "coverage" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "$AUB_BIN" coverage

    # 6. Doctor lists the classifications seen per account in the window.
    step "doctor" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "$AUB_BIN" doctor
}

case_assertions() {
    if [ -n "${ENDPOINT_429_PID:-}" ]; then
        kill "$ENDPOINT_429_PID" 2>/dev/null || true
        wait "$ENDPOINT_429_PID" 2>/dev/null || true
    fi
    if [ -n "${ENDPOINT_401_PID:-}" ]; then
        kill "$ENDPOINT_401_PID" 2>/dev/null || true
        wait "$ENDPOINT_401_PID" 2>/dev/null || true
    fi

    # Step 1: unreachable, and the command still exits zero.
    assert_exit 0 1
    assert_stdout_contains 1 "sample: account=codex-throttled outcome=unreachable"

    # Step 2: the stored classification is the provider's own error type, with
    # the sanitized message beside it in the one column.
    assert_exit 0 2
    assert_stdout_contains 2 "unreachable|rate_limit_error: Rate limit exceeded. Please retry later."

    # Step 3: auth required, still exit zero.
    assert_exit 0 3
    assert_stdout_contains 3 "sample: account=codex-rejected outcome=auth_required"

    # Step 4: the authentication spelling, with the message beside it.
    assert_exit 0 4
    assert_stdout_contains 4 "auth_required|authentication_error: Invalid authentication token provided."

    # Step 5: coverage names each classification with its count, one per
    # finding, grouped by classification rather than by message. The command
    # exits class 7 on purpose: a day with one attempt against a 300 s cadence
    # breaches the configured floors, and the breached threshold is exactly
    # the condition the findings are there to explain.
    assert_exit 7 5
    assert_stdout_contains 5 "1 attempt refused with rate_limit_error"
    assert_stdout_contains 5 "1 attempt refused with authentication_error"

    # Step 6: doctor lists the classifications seen per account.
    assert_exit 0 6
    assert_stdout_contains 6 "codex-throttled: rate_limit_error (count=1)"
    assert_stdout_contains 6 "codex-rejected: authentication_error (count=1)"
}