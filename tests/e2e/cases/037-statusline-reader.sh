# aub-gnke: the status-line reader. An Anthropic account whose record file
# holds a line younger than the ordinary cadence is observed from that line
# with no request to the usage endpoint; the endpoint runs only when no
# fresh line exists. The record file itself is written by the real `aub
# statusline` tee here, so the pipeline this case proves is the installed
# one: render, tee, read, ledger. The synthetic endpoint counts its requests
# to a file, which is what turns "the endpoint was not called" from an
# assertion about output into a measurement of the network.

CASE_ID="037-statusline-reader"
CASE_DESCRIPTION="aub now observes an account from a fresh status-line record with no endpoint request, falls back to the endpoint when the line is stale, and honours anthropic.statusline = false."

RECORD_FILE=""
LEDGER_DB=""
ENDPOINT_PORT=""
ENDPOINT_SERVER_PID=""

case_preconditions() {
    require_command "$AUB_BIN"
    require_command sqlite3
    require_command python3

    LEDGER_DB="$STATE_DIR/ledger.db"

    echo '{"accessToken":"test-token"}' > "$STATE_DIR/credential.json"

    # One config per source rule: the default one (statusline on) and the
    # recovery one (anthropic.statusline = false), over the same state dir.
    cat > "$STATE_DIR/aub.toml" <<CFG_EOF
[state]
dir = "$STATE_DIR"

[[accounts]]
name = "gmail"
provider = "anthropic"
credential = { kind = "file", path = "$STATE_DIR/credential.json" }
CFG_EOF
    cat > "$STATE_DIR/aub-statusline-off.toml" <<CFG_EOF
[state]
dir = "$STATE_DIR"

[anthropic]
statusline = false

[[accounts]]
name = "gmail"
provider = "anthropic"
credential = { kind = "file", path = "$STATE_DIR/credential.json" }
CFG_EOF
    RECORD_FILE="$STATE_DIR/statusline/gmail.jsonl"

    # The synthetic usage endpoint: answers every request with the committed
    # sanitized endpoint capture and counts each one, so the assertions
    # below read the request count off the server, not off the command.
    python3 -c "
import http.server, socketserver, sys

port_file, fixture, counter = sys.argv[1], sys.argv[2], sys.argv[3]

class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        with open(counter, 'a') as f:
            f.write('1\n')
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
" "$STATE_DIR/endpoint-port.txt" \
      "$REPO_ROOT/tests/fixtures/meter/anthropic/limits-success.json" \
      "$STATE_DIR/endpoint-hits.txt" &
    ENDPOINT_SERVER_PID=$!

    local count=0
    while [ ! -s "$STATE_DIR/endpoint-port.txt" ]; do
        sleep 0.05
        count=$((count + 1))
        if [ "$count" -gt 60 ]; then
            echo "timed out waiting for the endpoint stub server" >&2
            exit 1
        fi
    done
    ENDPOINT_PORT=$(cat "$STATE_DIR/endpoint-port.txt")
}

# statusline_step NAME PAYLOAD: pipes the payload through the real tee the
# way the installed status line does, with the profile naming the account.
statusline_step() {
    local name="$1" payload="$2"
    step "$name" env \
        "HOME=$STATE_DIR/home" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "SHALLOW_PROFILE=gmail" \
        sh -c 'cat "$1" | "$2" statusline' _ "$payload" "$AUB_BIN"
}

# now_step NAME CONFIG: one forced `aub now` tick against the synthetic
# endpoint, over the named config file.
now_step() {
    local name="$1" config="$2"
    step "$name" env \
        "HOME=$STATE_DIR/home" \
        "AUB_CONFIG_FILE=$config" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_ANTHROPIC_ENDPOINT=http://127.0.0.1:$ENDPOINT_PORT" \
        "$AUB_BIN" now --account gmail
}

# age_record SECONDS: rewrites the record's last line with `received_at`
# moved the given many seconds away from now - backwards for a positive
# value, forwards for a negative one. The tee wrote the line; only its
# receive stamp is aged, which is exactly what a stale file is.
age_record() {
    local seconds="$1"
    python3 - "$RECORD_FILE" "$seconds" <<'PYEOF'
import json, sys
from datetime import datetime, timedelta, timezone

path, seconds = sys.argv[1], int(sys.argv[2])
lines = [line for line in open(path).read().splitlines() if line.strip()]
row = json.loads(lines[-1])
received = datetime.now(timezone.utc) - timedelta(seconds=seconds)
row["received_at"] = received.strftime("%Y-%m-%dT%H:%M:%SZ")
lines[-1] = json.dumps(row)
open(path, "w").write("\n".join(lines) + "\n")
PYEOF
}

case_steps() {
    # 1. The tee writes the record from a real render, stamped with its own
    #    clock: the line the reader will find fresh seconds later.
    statusline_step "tee-records-the-render" "$REPO_ROOT/tests/fixtures/statusline/payload-five-seven.json"

    # 2. The tick finds the line fresh: one observation, no endpoint request.
    now_step "now-reads-the-fresh-line" "$STATE_DIR/aub.toml"

    # 3. Age the last line past the ordinary cadence.
    age_record 301

    # 4. The tick finds no fresh line: the endpoint answers.
    now_step "now-falls-back-to-the-endpoint" "$STATE_DIR/aub.toml"

    # 5. A fresh line with the feature off: the adapter ignores the record
    #    file and the endpoint runs anyway. This is the recovery path.
    age_record -5
    now_step "now-with-statusline-off" "$STATE_DIR/aub-statusline-off.toml"

    # 6. Both observations, with the contract each source declared.
    step "query-contracts" sqlite3 "$LEDGER_DB" \
        "SELECT o.provider_contract_id FROM meter_observation o JOIN account a ON a.id = o.account_id WHERE a.logical_name = 'gmail' ORDER BY o.id"

    # 7. The status-line observation's windows: the line's five_hour and
    #    seven_day under the shared vocabulary's names.
    step "query-statusline-windows" sqlite3 "$LEDGER_DB" \
        "SELECT w.semantic_key, w.scope_kind, w.quota_used_ppm, w.reset_state FROM meter_window w JOIN meter_observation o ON o.id = w.observation_id WHERE o.provider_contract_id = 'anthropic-statusline-rate-limits-v1' ORDER BY w.semantic_key"

    # 8. The status-line capsule: the windows map and the session, and no
    #    cwd - grepped by value from the fixture payload's own cwd.
    step "query-statusline-capsule" sqlite3 "$LEDGER_DB" \
        "SELECT e.evidence_capsule FROM meter_response_evidence e JOIN meter_observation o ON o.evidence_id = e.id WHERE o.provider_contract_id = 'anthropic-statusline-rate-limits-v1'"
}

case_assertions() {
    if [ -n "${ENDPOINT_SERVER_PID:-}" ]; then
        kill "$ENDPOINT_SERVER_PID" 2>/dev/null || true
        wait "$ENDPOINT_SERVER_PID" 2>/dev/null || true
    fi

    assert_exit 0 1
    assert_exit 0 2
    assert_exit 0 3
    assert_exit 0 4

    # The tee recorded exactly one line, attributed to gmail.
    if [ -s "$RECORD_FILE" ] && [ "$(wc -l < "$RECORD_FILE")" = "1" ]; then
        record_assertion "the tee wrote one record line" "1" "$(wc -l < "$RECORD_FILE")" "pass"
    else
        record_assertion "the tee wrote one record line" "1" "absent-or-more" "fail"
        CASE_FAILED=1
    fi

    # The endpoint was called exactly twice: once for the stale line, once
    # with the feature off. The fresh-line tick sent no request at all.
    local hits
    hits="$(wc -l < "$STATE_DIR/endpoint-hits.txt" 2>/dev/null || echo 0)"
    if [ "$hits" = "2" ]; then
        record_assertion "the endpoint served exactly the two stale and disabled ticks" "2" "$hits" "pass"
    else
        record_assertion "the endpoint served exactly the two stale and off ticks" "2" "$hits" "fail"
        CASE_FAILED=1
    fi

    # One observation per source, in order: the fresh tick's status-line
    # contract, then two endpoint contracts.
    assert_exit 0 5
    assert_stdout_contains 5 "anthropic-statusline-rate-limits-v1"
    assert_stdout_contains 5 "anthropic-oauth-usage-limits-v1"
    local contract_rows
    contract_rows="$(sed -n '1,3p' "$(step_dir 5)/stdout.txt" | tr -d '\r')"
    if [ "$(printf '%s\n' "$contract_rows" | wc -l)" = "3" ] \
        && [ "$(printf '%s\n' "$contract_rows" | sed -n 1p)" = "anthropic-statusline-rate-limits-v1" ] \
        && [ "$(printf '%s\n' "$contract_rows" | sed -n 2p)" = "anthropic-oauth-usage-limits-v1" ] \
        && [ "$(printf '%s\n' "$contract_rows" | sed -n 3p)" = "anthropic-oauth-usage-limits-v1" ]; then
        record_assertion "one contract per source, status line first" "statusline,limits,limits" "$contract_rows" "pass"
    else
        record_assertion "one contract per source, status line first" "statusline,limits,limits" "$contract_rows" "fail"
        CASE_FAILED=1
    fi

    # The status-line observation's windows: the line's payload names under
    # the shared vocabulary's names, both account-wide, both known resets.
    assert_exit 0 6
    assert_stdout_contains 6 "session|account_wide|400000|known"
    assert_stdout_contains 6 "weekly_all|account_wide|120000|known"

    # The status-line capsule never carries the payload's cwd.
    local capsule
    capsule="$(sed -n 1p "$(step_dir 7)/stdout.txt")"
    if [ -n "$capsule" ] && ! printf '%s' "$capsule" | grep -qF "/tmp/worktree/project"; then
        record_assertion "the status-line capsule carries no cwd" "absent" "absent" "pass"
    else
        record_assertion "the status-line capsule carries no cwd" "absent" "present" "fail"
        CASE_FAILED=1
    fi
}