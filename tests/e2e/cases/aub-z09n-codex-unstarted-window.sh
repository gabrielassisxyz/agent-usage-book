# aub-z09n: an unstarted Codex window claims no reset instant. When a Codex
# window has not been used, the provider answers with a reset exactly one
# nominal duration after the request, so the stored anchor used to slide one
# sampling interval per tick and every consecutive pair of idle readings
# wrote an `unexpected_reset_change` row into the anomaly evidence. The
# adapter now stores such a reading as `not_started` with no instant at all,
# so ten consecutive idle ticks against a provider that answers with the
# fabricated shape every time must record zero `meter_window_anomaly` rows
# and store both windows with an empty `resets_at`, while one final tick
# whose boundary has genuinely moved far past the stated tolerance, with
# real usage on the window, must still record the typed anomaly for every
# window. The rollout path is covered beside the endpoint path: a home that
# owns its sessions tree reads the same fabricated shape from a rollout
# whose reset is anchored to the file's own pinned mtime, and it must store
# `not_started` the same way. `aub status` must render the idle window the
# way it already renders any other unstarted window.

CASE_ID="aub-z09n-codex-unstarted-window"
CASE_DESCRIPTION="ten idle codex endpoint ticks record zero window anomalies and store both windows unstarted, a genuinely moved boundary with real usage still records unexpected_reset_change, an owning rollout home stores its unstarted window the same way, and status renders the idle window as not started."

LEDGER_DB=""
IDLE_STUB_PID=""
MOVED_STUB_PID=""
IDLE_PORT=""
MOVED_PORT=""
ROLLOUT_MTIME=""

case_preconditions() {
    require_command "$AUB_BIN"
    require_command sqlite3
    require_command python3

    LEDGER_DB="$STATE_DIR/ledger.db"

    # The owning home: its newest rollout carries the fabricated unstarted
    # shape, zero usage with a reset exactly one nominal duration after the
    # file's own mtime, which is the reading's measurement instant on this
    # path. The mtime is taken at case setup and pinned on the file, so the
    # fixture reproduces the shape exactly however long the run is delayed.
    ROLLOUT_MTIME=$(date +%s)
    mkdir -p "$STATE_DIR/codex-home/sessions/2026/09/08"
    cp "$REPO_ROOT/tests/fixtures/meter/codex/auth-fixture.json" \
        "$STATE_DIR/codex-home/auth.json"
    cat > "$STATE_DIR/codex-home/sessions/2026/09/08/rollout-idle-session.jsonl" <<ROLLOUT_EOF
{"timestamp":"2026-09-08T12:00:00.000Z","type":"event_msg","payload":{"type":"token_count","rate_limits":{"limit_id":"codex","primary":{"used_percent":0,"window_minutes":300,"resets_at":$((ROLLOUT_MTIME + 300 * 60))},"secondary":{"used_percent":0,"window_minutes":10080,"resets_at":$((ROLLOUT_MTIME + 10080 * 60))}}}}
ROLLOUT_EOF
    touch -d @"$ROLLOUT_MTIME" \
        "$STATE_DIR/codex-home/sessions/2026/09/08/rollout-idle-session.jsonl"

    # The shared home: its sessions entry is a symlink to a tree it does not
    # own, the production shape for an endpoint account, so the adapter reads
    # the usage endpoint with the account's own credential and never opens a
    # rollout.
    mkdir -p "$STATE_DIR/shared-sessions/2026/09/08"
    mkdir -p "$STATE_DIR/codex-shared-home"
    cp "$REPO_ROOT/tests/fixtures/meter/codex/auth-fixture.json" \
        "$STATE_DIR/codex-shared-home/auth.json"
    ln -s "$STATE_DIR/shared-sessions" "$STATE_DIR/codex-shared-home/sessions"

    cat > "$STATE_DIR/aub.toml" <<CFG_EOF
state.dir = "$STATE_DIR"

[[accounts]]
name = "codex-idle-endpoint"
provider = "codex"
credential = { kind = "file", path = "$STATE_DIR/codex-shared-home/auth.json" }
codex_home = "$STATE_DIR/codex-shared-home"

[[accounts]]
name = "codex-idle-rollout"
provider = "codex"
credential = { kind = "file", path = "$STATE_DIR/codex-home/auth.json" }
codex_home = "$STATE_DIR/codex-home"
CFG_EOF

    # Two stub transports, the way the live provider behaves: the idle one
    # answers every request with the fabricated unstarted shape, the reset
    # exactly one nominal duration after the request instant, and the moved
    # one with a boundary pushed four hundred hours past that shape and real
    # usage on both windows. The fabricated shape is computed per request
    # from the stub's own clock, which is what makes ten ticks slide the
    # anchor the way the production evidence showed.
    python3 -c "
import http.server
import socketserver
import sys
import time
import json

port_file = sys.argv[1]
reset_offset_seconds = int(sys.argv[2])
primary_used_percent = int(sys.argv[3])
secondary_used_percent = int(sys.argv[4])

class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        now = int(time.time())
        body = json.dumps({
            'rate_limit': {
                'primary_window': {
                    'used_percent': primary_used_percent,
                    'limit_window_seconds': 18000,
                    'reset_at': now + 18000 + reset_offset_seconds,
                },
                'secondary_window': {
                    'used_percent': secondary_used_percent,
                    'limit_window_seconds': 604800,
                    'reset_at': now + 604800 + reset_offset_seconds,
                },
            }
        }).encode('utf-8')
        self.send_response(200)
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
" "$STATE_DIR/idle-port.txt" 0 0 0 &
    IDLE_STUB_PID=$!
    python3 -c "
import http.server
import socketserver
import sys
import time
import json

port_file = sys.argv[1]
reset_offset_seconds = int(sys.argv[2])
primary_used_percent = int(sys.argv[3])
secondary_used_percent = int(sys.argv[4])

class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        now = int(time.time())
        body = json.dumps({
            'rate_limit': {
                'primary_window': {
                    'used_percent': primary_used_percent,
                    'limit_window_seconds': 18000,
                    'reset_at': now + 18000 + reset_offset_seconds,
                },
                'secondary_window': {
                    'used_percent': secondary_used_percent,
                    'limit_window_seconds': 604800,
                    'reset_at': now + 604800 + reset_offset_seconds,
                },
            }
        }).encode('utf-8')
        self.send_response(200)
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
" "$STATE_DIR/moved-port.txt" 1440000 42 27 &
    MOVED_STUB_PID=$!

    local count=0
    while [ ! -s "$STATE_DIR/idle-port.txt" ] || [ ! -s "$STATE_DIR/moved-port.txt" ]; do
        sleep 0.05
        count=$((count + 1))
        if [ "$count" -gt 60 ]; then
            echo "timed out waiting for the codex stub servers" >&2
            exit 1
        fi
    done
    IDLE_PORT=$(cat "$STATE_DIR/idle-port.txt")
    MOVED_PORT=$(cat "$STATE_DIR/moved-port.txt")
}

case_steps() {
    # Ten consecutive idle ticks against the fabricated shape, the shape the
    # live provider answers for an unused window. Every tick's reported
    # anchor is one nominal duration after that tick's own instant, so in
    # the old representation every consecutive pair slid by the sampling
    # interval. Stored as not_started, the pair is idle to idle.
    #
    # The ticks run back to back on purpose. The decision under test is made
    # per reading, never per pair: `reinterpret_unstarted_window`
    # (src/meter/codex.rs) rewrites a zero-usage window whose reported reset
    # sits within the 2 s jitter envelope of `observed_at + nominal_duration`
    # into `not_started` before any pair comparison, and two `not_started`
    # readings are never an anomaly whatever the gap between them. So the
    # spacing of the ticks decides nothing about the zero-anomaly assertion,
    # and the assertion that tells the old representation from the new one
    # is the stored shape in step 12: `Known` anchors with a `resets_at`
    # fail it at any spacing, `not_started` with an empty `resets_at` passes.
    # This case used to sleep four seconds per tick to push each pair's slide
    # past the envelope; that made the old code fail the anomaly count too,
    # at a cost of 40 s per run, and proved nothing step 12 does not.
    local tick
    for tick in 01 02 03 04 05 06 07 08 09 10; do
        step "tick-$tick-codex-idle-sample" env \
            "HOME=$STATE_DIR/home" \
            "AUB_STATE_DIR=$STATE_DIR" \
            "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
            "AUB_CODEX_ENDPOINT=http://127.0.0.1:$IDLE_PORT" \
            "$AUB_BIN" sample --account codex-idle-endpoint
    done

    # The ledger must hold zero window anomalies after the ten ticks.
    step "query-anomaly-count-after-ten-ticks" sqlite3 "$LEDGER_DB" \
        "SELECT 'anomaly_count=' || count(*) FROM meter_window_anomaly"

    # The account's latest observation must store both windows unstarted,
    # with no reset instant invented for either.
    step "query-idle-endpoint-windows" sqlite3 "$LEDGER_DB" \
        "SELECT w.semantic_key || '|' || coalesce(w.resets_at, '') || '|' || w.reset_state || '|' || w.quota_used_ppm FROM meter_window w JOIN meter_observation o ON o.id = w.observation_id JOIN account a ON a.id = o.account_id WHERE a.logical_name = 'codex-idle-endpoint' AND o.id = (SELECT max(o2.id) FROM meter_observation o2 JOIN account a2 ON a2.id = o2.account_id WHERE a2.logical_name = 'codex-idle-endpoint') ORDER BY w.semantic_key"

    # Status renders the idle windows the way it renders any unstarted
    # window, with no reset instant invented for them.
    step "status-after-idle-ticks" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "$AUB_BIN" status

    # The rollout path: an owning home whose rollout carries the same
    # fabricated shape anchored to the file's pinned mtime.
    step "sample-rollout-idle-account" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "$AUB_BIN" sample --account codex-idle-rollout

    step "query-rollout-windows" sqlite3 "$LEDGER_DB" \
        "SELECT w.semantic_key || '|' || coalesce(w.resets_at, '') || '|' || w.reset_state || '|' || w.quota_used_ppm || '|' || w.nominal_duration_nanos FROM meter_window w JOIN meter_observation o ON o.id = w.observation_id JOIN account a ON a.id = o.account_id WHERE a.logical_name = 'codex-idle-rollout' ORDER BY w.semantic_key"

    # One sample whose boundary has genuinely moved four hundred hours past
    # the fabricated shape, with real usage on both windows: past the stated
    # tolerance for every window, so each records the typed
    # unexpected-reset-change anomaly against the idle reading before it.
    step "sample-after-real-reset-move" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "AUB_CODEX_ENDPOINT=http://127.0.0.1:$MOVED_PORT" \
        "$AUB_BIN" sample --account codex-idle-endpoint

    # The anomaly kind counts: exactly the real move's two rows, one per
    # window, and nothing else.
    step "query-anomaly-kinds-after-real-move" sqlite3 "$LEDGER_DB" \
        "SELECT kind || '=' || count(*) FROM meter_window_anomaly GROUP BY kind"
}

case_assertions() {
    # The two stub transports answer forever by design, so they are stopped
    # before the assertions read their steps: a server still holding the
    # runner's stdout would keep the run open after the case is done.
    if [ -n "${IDLE_STUB_PID:-}" ]; then
        kill "$IDLE_STUB_PID" 2>/dev/null || true
        wait "$IDLE_STUB_PID" 2>/dev/null || true
    fi
    if [ -n "${MOVED_STUB_PID:-}" ]; then
        kill "$MOVED_STUB_PID" 2>/dev/null || true
        wait "$MOVED_STUB_PID" 2>/dev/null || true
    fi

    # Steps 1 through 10 are the ten idle ticks, in order.
    local n
    for n in 1 2 3 4 5 6 7 8 9 10; do
        assert_exit 0 "$n"
        assert_stdout_contains "$n" "sample: account=codex-idle-endpoint outcome=success"
    done

    # Step 11: ten ticks of the sliding fabricated shape recorded no anomaly
    # of any kind.
    assert_exit 0 11
    assert_stdout_contains 11 "anomaly_count=0"

    # Step 12: both windows of the latest idle observation are stored
    # unstarted, with an empty resets_at column.
    assert_exit 0 12
    assert_stdout_contains 12 "primary||not_started|0"
    assert_stdout_contains 12 "secondary||not_started|0"

    # Step 13: status shows the account and renders its idle windows as not
    # started.
    assert_exit 0 13
    assert_stdout_contains 13 "codex-idle-endpoint"
    assert_stdout_contains 13 "not started"

    # Step 14: the owning home samples successfully from its rollout.
    assert_exit 0 14
    assert_stdout_contains 14 "sample: account=codex-idle-rollout outcome=success"

    # Step 15: the rollout path stores the unstarted window the same way,
    # with the nominal durations the record carries.
    assert_exit 0 15
    assert_stdout_contains 15 "primary||not_started|0|18000000000000"
    assert_stdout_contains 15 "secondary||not_started|0|604800000000000"

    # Step 16 ran, and step 17 shows the real move still records one anomaly
    # per window, named by kind.
    assert_exit 0 16
    assert_exit 0 17
    assert_stdout_contains 17 "unexpected_reset_change=2"
}