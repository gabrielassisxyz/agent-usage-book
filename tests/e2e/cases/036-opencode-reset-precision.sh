# aub-w1a0: the opencode reset anchor is derived from a page text that floors
# the remaining time to whole hours, so re-deriving it on every tick drifts
# the instant by the sampling interval without the provider having moved
# anything. The adapter declares a one-hour reset precision, and the
# window-anomaly classifier holds a `Known`-to-`Known` move within that
# precision plus the gap between the observations to be one unchanged
# boundary. Ten consecutive samples of an unchanging page must therefore
# record zero `meter_window_anomaly` rows, while one sample whose page really
# moved the boundary far past that tolerance must still record the typed
# `unexpected_reset_change` anomaly for every window.

CASE_ID="036-opencode-reset-precision"
CASE_DESCRIPTION="ten consecutive opencode samples of an unchanging page record zero window anomalies, and a real boundary move still records unexpected_reset_change."

LEDGER_DB=""
STABLE_STUB_PID=""
MOVED_STUB_PID=""
STABLE_PORT=""
MOVED_PORT=""
OPENCODE_COOKIE_MATERIAL=""

case_preconditions() {
    require_command "$AUB_BIN"
    require_command sqlite3
    require_command python3

    LEDGER_DB="$STATE_DIR/ledger.db"

    # Distinctive on purpose, so a leak grep matches nothing but a leak: the
    # bare cookie value the adapter builds its `auth=<value>` header from.
    OPENCODE_COOKIE_MATERIAL="fixture-session-cookie-9f2c-not-a-real-value"

    mkdir -p "$STATE_DIR/home"

    cat > "$STATE_DIR/aub.toml" <<CFG_EOF
state.dir = "$STATE_DIR"

[[accounts]]
name = "go-primary"
provider = "opencode"
credential = { kind = "env", name = "OPENCODE_SESSION_COOKIE" }
opencode_workspace = "wrk_2345ABCDEFGHJKLMNOPQRSTuvwx"
CFG_EOF

    # The negative control's page: the committed fixture with every reset
    # text moved far into the future, so one sample against it moves each
    # window's derived boundary by hundreds of hours and days, far past the
    # declared one-hour precision plus any observation gap.
    python3 - "$REPO_ROOT/tests/fixtures/meter/opencode/valid.html" \
        "$STATE_DIR/moved.html" <<'PY_EOF'
import sys

source, destination = sys.argv[1], sys.argv[2]
page = open(source).read()
page = page.replace("5 hours 0 minutes", "500 hours 0 minutes")
page = page.replace("6 days 8 hours", "660 days 8 hours")
page = page.replace("27 days 8 hours", "670 days 8 hours")
open(destination, "w").write(page)
PY_EOF

    # Two stub transports: the stable one answers every request with the
    # committed fixture (tick-to-tick derived moves are the sampling gap
    # only), the moved one with the shifted page.
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
" "$STATE_DIR/stable-port.txt" "$REPO_ROOT/tests/fixtures/meter/opencode/valid.html" &
    STABLE_STUB_PID=$!
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
" "$STATE_DIR/moved-port.txt" "$STATE_DIR/moved.html" &
    MOVED_STUB_PID=$!

    local count=0
    while [ ! -s "$STATE_DIR/stable-port.txt" ] || [ ! -s "$STATE_DIR/moved-port.txt" ]; do
        sleep 0.05
        count=$((count + 1))
        if [ "$count" -gt 60 ]; then
            echo "timed out waiting for the opencode stub servers" >&2
            exit 1
        fi
    done
    STABLE_PORT=$(cat "$STATE_DIR/stable-port.txt")
    MOVED_PORT=$(cat "$STATE_DIR/moved-port.txt")
}

case_steps() {
    # Each tick waits four seconds before sampling, on purpose: the sampling
    # process is fast against a local stub, so back-to-back ticks re-derive
    # resets only fractions of a second apart and the fixed 2 s provider-jitter
    # envelope would absorb the drift with no declaration at all. Four seconds
    # of gap puts the drift past the fixed envelope - the declared precision is
    # what has to absorb it - while staying far inside that precision plus the
    # gap, which is the tolerance aub-w1a0 declares.
    local tick
    for tick in 01 02 03 04 05 06 07 08 09 10; do
        sleep 4
        step "tick-$tick-opencode-sample" env \
            "HOME=$STATE_DIR/home" \
            "AUB_STATE_DIR=$STATE_DIR" \
            "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
            "AUB_OPENCODE_ENDPOINT=http://127.0.0.1:$STABLE_PORT" \
            "OPENCODE_SESSION_COOKIE=$OPENCODE_COOKIE_MATERIAL" \
            "$AUB_BIN" sample --account go-primary
    done

    # The ledger must hold zero window anomalies after the ten ticks.
    step "query-anomaly-count-after-ten-ticks" sqlite3 "$LEDGER_DB" \
        "SELECT 'anomaly_count=' || count(*) FROM meter_window_anomaly"

    # One sample against the page whose rendered resets moved by hundreds of
    # hours and days: past the declared precision plus the gap for every
    # window, with the quota percentages unchanged, so each of the three
    # windows records the typed unexpected-reset-change anomaly.
    step "sample-after-real-reset-move" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "AUB_OPENCODE_ENDPOINT=http://127.0.0.1:$MOVED_PORT" \
        "OPENCODE_SESSION_COOKIE=$OPENCODE_COOKIE_MATERIAL" \
        "$AUB_BIN" sample --account go-primary

    # The anomaly kind counts: exactly the real move's three rows, one per
    # window, and nothing else.
    step "query-anomaly-kinds-after-real-move" sqlite3 "$LEDGER_DB" \
        "SELECT kind || '=' || count(*) FROM meter_window_anomaly GROUP BY kind"
}

case_assertions() {
    # The two stub transports answer forever by design, so they are stopped
    # before the assertions read their steps: a server still holding the
    # runner's stdout would keep the run open after the case is done.
    if [ -n "${STABLE_STUB_PID:-}" ]; then
        kill "$STABLE_STUB_PID" 2>/dev/null || true
        wait "$STABLE_STUB_PID" 2>/dev/null || true
    fi
    if [ -n "${MOVED_STUB_PID:-}" ]; then
        kill "$MOVED_STUB_PID" 2>/dev/null || true
        wait "$MOVED_STUB_PID" 2>/dev/null || true
    fi

    # Steps 1 through 10 are the ten ticks, in order.
    local n
    for n in 1 2 3 4 5 6 7 8 9 10; do
        assert_exit 0 "$n"
        assert_stdout_contains "$n" "sample: account=go-primary outcome=success"
    done

    # Step 11: ten ticks of an unchanging page recorded no anomaly of any kind.
    assert_exit 0 11
    assert_stdout_contains 11 "anomaly_count=0"

    # Step 12 ran, and step 13 shows the real move still records one anomaly
    # per window, named by kind.
    assert_exit 0 12
    assert_exit 0 13
    assert_stdout_contains 13 "unexpected_reset_change=3"
}