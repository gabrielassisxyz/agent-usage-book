# An account whose credential the provider keeps rejecting backs off
# (aub-x2je): after the configured consecutive `auth_required` streak the
# next tick reports `not-due` with no new attempt, a rewritten credential
# resumes at once, and `aub coverage` does not count the hold as a miss.
#
# Threshold two with a one-second cadence keeps the run short: the first two
# ticks sample (streaks zero and one), the third holds (streak two), the
# credential rewrite resumes the fourth, and coverage over the window stays
# whole because the hold was not owed.

CASE_ID="aub-x2je-auth-backoff"
CASE_DESCRIPTION="a rejected credential backs off after the streak, resumes on rewrite, and coverage does not count the hold as missed."

LEDGER_DB=""
ANTHROPIC_PORT=""
ANTHROPIC_SERVER_PID=""

case_preconditions() {
    require_command "$AUB_BIN"
    require_command sqlite3
    require_command python3

    LEDGER_DB="$STATE_DIR/ledger.db"

    mkdir -p "$STATE_DIR/home" "$STATE_DIR/creds"
    echo '{"accessToken":"dead-token-1"}' > "$STATE_DIR/creds/dead.json"

    cat > "$STATE_DIR/aub.toml" <<CFG_EOF
state.dir = "$STATE_DIR"

[sampling]
default_interval = "1s"
auth_backoff_threshold = 2
auth_backoff_cap = "60s"

[coverage]
attempt_floor = 0.50
measurement_floor = 0.0

[[accounts]]
name = "dead-primary"
provider = "anthropic"
credential = { kind = "file", path = "$STATE_DIR/creds/dead.json" }
CFG_EOF

    # The synthetic Anthropic transport: every request is a 401 with the
    # provider's authentication error body, so the adapter concludes
    # authentication is required.
    python3 -c "
import http.server
import socketserver
import sys

port_file = sys.argv[1]

class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        body = b'{\"type\":\"error\",\"error\":{\"type\":\"authentication_error\",\"message\":\"Invalid authentication token provided.\"}}'
        self.send_response(401)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def log_message(self, format, *args):
        pass

httpd = socketserver.TCPServer(('127.0.0.1', 0), Handler)
with open(port_file, 'w') as pf:
    pf.write(str(httpd.server_address[1]))
httpd.serve_forever()
" "$STATE_DIR/anthropic-port.txt" &
    ANTHROPIC_SERVER_PID=$!

    local count=0
    while [ ! -s "$STATE_DIR/anthropic-port.txt" ]; do
        sleep 0.05
        count=$((count + 1))
        if [ "$count" -gt 60 ]; then
            echo "timed out waiting for the anthropic stub server" >&2
            exit 1
        fi
    done
    ANTHROPIC_PORT=$(cat "$STATE_DIR/anthropic-port.txt")
}

case_steps() {
    # 1. First tick: no history, so due; the stub rejects it.
    step "sample-due-first-tick-rejected" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "AUB_ANTHROPIC_ENDPOINT=http://127.0.0.1:$ANTHROPIC_PORT" \
        "$AUB_BIN" sample --due

    # 2. Past the 1 s cadence boundary, still below the threshold of two.
    step "hold-past-cadence-boundary" sleep 2

    # 3. Second tick: streak one, still due; rejected again.
    step "sample-due-second-tick-rejected" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "AUB_ANTHROPIC_ENDPOINT=http://127.0.0.1:$ANTHROPIC_PORT" \
        "$AUB_BIN" sample --due

    # 4. Past the boundary again, now inside the backoff hold.
    step "hold-past-second-boundary" sleep 2

    # 5. Third tick: streak two reaches the threshold, so not-due with no
    #    new attempt.
    step "sample-due-third-tick-held" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "AUB_ANTHROPIC_ENDPOINT=http://127.0.0.1:$ANTHROPIC_PORT" \
        "$AUB_BIN" sample --due

    # 6. Still exactly two attempts: the held tick issued no request.
    step "query-attempt-count-after-hold" sqlite3 "$LEDGER_DB" "SELECT count(*) FROM meter_attempt"

    # 7. The operator replaces the credential: new bytes and a new mtime,
    #    so the context the next tick resolves differs.
    step "rewrite-credential" bash -c 'echo "{\"accessToken\":\"dead-token-2\"}" > "$1/creds/dead.json"; sleep 0.1' _ "$STATE_DIR"

    # 8. Fourth tick: the rewritten credential resumes at once.
    step "sample-due-fourth-tick-resumed" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "AUB_ANTHROPIC_ENDPOINT=http://127.0.0.1:$ANTHROPIC_PORT" \
        "$AUB_BIN" sample --due

    # 9. Three attempts now: the two rejections plus the resumed one.
    step "query-attempt-count-after-resume" sqlite3 "$LEDGER_DB" "SELECT count(*) FROM meter_attempt"

    # 10. The snapshot spells both rules in force.
    step "query-backoff-policy" sqlite3 -separator ',' "$LEDGER_DB" \
        "SELECT DISTINCT retry_backoff_policy FROM sampling_policy_snapshot"

    # 11. Coverage over the window: the hold was not owed, so attempt
    #     coverage does not read it as a shortfall.
    step "coverage-after-backoff" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "AUB_LOG_LEVEL=off" \
        "COLUMNS=80" \
        "$AUB_BIN" coverage --since 5m
}

case_assertions() {
    if [ -n "${ANTHROPIC_SERVER_PID:-}" ]; then
        kill "$ANTHROPIC_SERVER_PID" 2>/dev/null || true
        wait "$ANTHROPIC_SERVER_PID" 2>/dev/null || true
    fi

    # Steps 1 and 3: both ticks sample and report the rejection.
    assert_exit 0 1
    assert_stdout_contains 1 "sample: account=dead-primary outcome=auth_required"
    assert_exit 0 3
    assert_stdout_contains 3 "sample: account=dead-primary outcome=auth_required"

    # Step 5: the streak holds the account past the cadence boundary.
    assert_exit 0 5
    assert_stdout_contains 5 "sample: account=dead-primary not-due"

    # Step 6: the held tick created no attempt row.
    assert_exit 0 6
    if grep -qx "2" "$(step_dir 6)/stdout.txt"; then
        record_assertion "attempt count after hold" "2" "$(cat "$(step_dir 6)/stdout.txt")" "pass"
    else
        record_assertion "attempt count after hold" "2" "$(cat "$(step_dir 6)/stdout.txt")" "fail"
        CASE_FAILED=1
    fi

    # Step 8: the rewritten credential resumes at the next tick.
    assert_exit 0 8
    assert_stdout_contains 8 "sample: account=dead-primary outcome=auth_required"

    # Step 9: the resumed tick is the third attempt row.
    assert_exit 0 9
    if grep -qx "3" "$(step_dir 9)/stdout.txt"; then
        record_assertion "attempt count after resume" "3" "$(cat "$(step_dir 9)/stdout.txt")" "pass"
    else
        record_assertion "attempt count after resume" "3" "$(cat "$(step_dir 9)/stdout.txt")" "fail"
        CASE_FAILED=1
    fi

    # Step 10: the snapshot carries both the Retry-After cap and the
    # authentication threshold and cap, not a constant.
    assert_exit 0 10
    if grep -qx "retry-after-capped-3600s auth-2-60s" "$(step_dir 10)/stdout.txt"; then
        record_assertion "retry_backoff_policy" "retry-after-capped-3600s auth-2-60s" \
            "$(cat "$(step_dir 10)/stdout.txt")" "pass"
    else
        record_assertion "retry_backoff_policy" "retry-after-capped-3600s auth-2-60s" \
            "$(cat "$(step_dir 10)/stdout.txt")" "fail"
        CASE_FAILED=1
    fi

    # Step 11: coverage distinguishes the hold (not owed) from a miss: the
    # account renders with its numbers and the command does not report the
    # hold as a coverage shortfall below the 50% floor.
    assert_exit 0 11
    assert_stdout_contains 11 "dead-primary"
}
