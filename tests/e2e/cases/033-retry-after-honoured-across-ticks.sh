# A 429 with a Retry-After holds the account across the scheduler's ticks
# (aub-6w85): the second `aub sample --due`, whose cadence boundary has
# already passed, must report `not-due` with `next_due_at` at the refusal's
# finish plus `min(header, sampling.retry_after_cap)`, and must issue no
# request. One account carries a header at the cap's boundary (3600 s, the
# default ceiling) and one above it (9999 s, clamped), so both sides of
# `min` are pinned by the same run.

CASE_ID="033-retry-after-honoured-across-ticks"
CASE_DESCRIPTION="a 429's Retry-After holds the account not-due across ticks, capped, with no second attempt inside the lockout."

LEDGER_DB=""
ANTHROPIC_PORT=""
ANTHROPIC_SERVER_PID=""

case_preconditions() {
    require_command "$AUB_BIN"
    require_command sqlite3
    require_command python3

    LEDGER_DB="$STATE_DIR/ledger.db"

    mkdir -p "$STATE_DIR/home" "$STATE_DIR/creds"
    # Distinct tokens so one stub server can tell the accounts apart and
    # answer each with its own Retry-After.
    echo '{"accessToken":"lock-token-3600"}' > "$STATE_DIR/creds/lock.json"
    echo '{"accessToken":"clamp-token-9999"}' > "$STATE_DIR/creds/clamp.json"

    cat > "$STATE_DIR/aub.toml" <<CFG_EOF
state.dir = "$STATE_DIR"

[sampling]
# The second tick must run after the ordinary cadence boundary has already
# passed: a hold that only lasted the remaining cadence would not catch the
# defect this case pins, where every tick past the boundary re-attempted.
default_interval = "1s"

[[accounts]]
name = "lock-primary"
provider = "anthropic"
credential = { kind = "file", path = "$STATE_DIR/creds/lock.json" }

[[accounts]]
name = "clamp-primary"
provider = "anthropic"
credential = { kind = "file", path = "$STATE_DIR/creds/clamp.json" }
CFG_EOF

    # The synthetic Anthropic transport: a 429 whose Retry-After depends on
    # which account's bearer token the request carried.
    python3 -c "
import http.server
import socketserver
import sys

port_file = sys.argv[1]

class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        auth = self.headers.get('Authorization', '')
        retry_after = 9999 if 'clamp-token' in auth else 3600
        body = b'{\"error\":{\"type\":\"rate_limit_error\"}}'
        self.send_response(429)
        self.send_header('Retry-After', str(retry_after))
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
    # 1. First tick: both accounts have no history, so both are due, and the
    #    stub refuses both with their Retry-After.
    step "sample-due-first-tick-refused" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "AUB_ANTHROPIC_ENDPOINT=http://127.0.0.1:$ANTHROPIC_PORT" \
        "$AUB_BIN" sample --due

    # 2. Both refusals are recorded with the header each account received.
    step "query-retry-after-nanos" sqlite3 -separator ',' "$LEDGER_DB" \
        "SELECT acc.logical_name, r.retry_after_nanos FROM meter_attempt a JOIN account acc ON acc.id = a.account_id JOIN meter_attempt_result r ON r.attempt_id = a.id ORDER BY acc.logical_name"

    # 3. Hold past the 1 s cadence boundary, well inside the lockout.
    step "hold-past-cadence-boundary" sleep 2

    # 4. Second tick: the boundary has passed, the headers still hold.
    step "sample-due-second-tick-held" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "AUB_ANTHROPIC_ENDPOINT=http://127.0.0.1:$ANTHROPIC_PORT" \
        "$AUB_BIN" sample --due

    # 5. No request went out inside the lockout: still exactly two attempts.
    step "query-attempt-count" sqlite3 "$LEDGER_DB" "SELECT count(*) FROM meter_attempt"

    # 6. The config key prints with its default.
    step "config-shows-retry-after-cap" env \
        "HOME=$STATE_DIR/home" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "$AUB_BIN" config

    # 7. The policy snapshot spells the rule in force.
    step "query-retry-backoff-policy" sqlite3 -separator ',' "$LEDGER_DB" \
        "SELECT DISTINCT retry_backoff_policy FROM sampling_policy_snapshot"
}

case_assertions() {
    if [ -n "${ANTHROPIC_SERVER_PID:-}" ]; then
        kill "$ANTHROPIC_SERVER_PID" 2>/dev/null || true
        wait "$ANTHROPIC_SERVER_PID" 2>/dev/null || true
    fi

    # Step 1: the first tick samples both and exits 0 on the refusals.
    assert_exit 0 1
    assert_stdout_contains 1 "sample: account=lock-primary outcome=unreachable"
    assert_stdout_contains 1 "sample: account=clamp-primary outcome=unreachable"

    # Step 2: each refusal recorded with its own header, in nanoseconds.
    assert_exit 0 2
    assert_stdout_contains 2 "lock-primary,3600000000000"
    assert_stdout_contains 2 "clamp-primary,9999000000000"

    # Step 4: both accounts report not-due, and each next_due_at is its own
    # refusal's finish plus the capped 3600 s - the clamp account's header
    # said 9999 s, so a verbatim reading of the header would print
    # 9999000000000 there.
    assert_exit 0 4
    assert_stdout_contains 4 "sample: account=lock-primary not-due"
    assert_stdout_contains 4 "sample: account=clamp-primary not-due"
    local account expected completed next_due
    for account in lock-primary clamp-primary; do
        completed="$(sqlite3 -readonly "$LEDGER_DB" \
            "SELECT r.completed_at FROM meter_attempt a JOIN account acc ON acc.id = a.account_id JOIN meter_attempt_result r ON r.attempt_id = a.id WHERE acc.logical_name = '$account'")"
        next_due="$(
            grep -F "account=$account not-due" "$(step_dir 4)/stdout.txt" \
                | head -n 1 | sed 's/.*next_due_at=//'
        )"
        expected=$((completed + 3600000000000))
        if [ "$next_due" = "$expected" ]; then
            record_assertion "$account next_due_at" "refusal + 3600s" "$next_due" "pass"
        else
            record_assertion "$account next_due_at" "$expected" "$next_due" "fail"
            CASE_FAILED=1
        fi
    done

    # Step 5: the held tick created no attempt row.
    assert_exit 0 5
    if grep -qx "2" "$(step_dir 5)/stdout.txt"; then
        record_assertion "attempt count" "2" "$(cat "$(step_dir 5)/stdout.txt")" "pass"
    else
        record_assertion "attempt count" "2" "$(cat "$(step_dir 5)/stdout.txt")" "fail"
        CASE_FAILED=1
    fi

    # Step 6: the cap prints with its 3600 s default, rendered 1h.
    assert_exit 0 6
    if grep -Eq "retry_after_cap +1h +default" "$(step_dir 6)/stdout.txt"; then
        record_assertion "retry_after_cap row" "retry_after_cap 1h default" "row present" "pass"
    else
        record_assertion "retry_after_cap row" "retry_after_cap 1h default" "row absent" "fail"
        CASE_FAILED=1
    fi

    # Step 7: the snapshot records the rule, not a constant none.
    assert_exit 0 7
    if grep -qx "retry-after-capped-3600s auth-3-21600s" "$(step_dir 7)/stdout.txt"; then
        record_assertion "retry_backoff_policy" "retry-after-capped-3600s auth-3-21600s" \
            "$(cat "$(step_dir 7)/stdout.txt")" "pass"
    else
        record_assertion "retry_backoff_policy" "retry-after-capped-3600s auth-3-21600s" \
            "$(cat "$(step_dir 7)/stdout.txt")" "fail"
        CASE_FAILED=1
    fi
}