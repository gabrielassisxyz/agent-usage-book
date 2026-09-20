# aub-w1a0, revised by aub-id41: the opencode reset anchor used to be derived
# from a page text that floored the remaining time to whole hours, so
# re-deriving it on every tick drifted the instant by the sampling interval
# without the provider having moved anything, and this case existed to prove
# the declared one-hour precision absorbed that drift. The console status
# endpoint states an absolute instant instead, so the drift is gone at the
# source and the three ticks no longer need to be spaced past a jitter
# envelope to be meaningful.
#
# The case is kept rather than retired because its second half never depended
# on the derivation: the window-anomaly classifier must still record zero
# anomalies across consecutive samples of an unchanging response, and must
# still record the typed `unexpected_reset_change` for every window when the
# provider really moves a boundary. That is the property a re-derivation bug
# or an over-eager classifier would break, and nothing else asserts it
# end to end.

CASE_ID="036-opencode-reset-precision"
CASE_DESCRIPTION="three opencode samples of an unchanging status response record zero window anomalies, and a real boundary move still records unexpected_reset_change."

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
    # whole `Cookie` header value, carrying both cookies the console requires,
    # which the adapter sends verbatim.
    OPENCODE_COOKIE_MATERIAL="auth=fixture-session-cookie-9f2c-not-a-real-value; __Host-console_session=fixture-console-7b1d"

    mkdir -p "$STATE_DIR/home"

    cat > "$STATE_DIR/aub.toml" <<CFG_EOF
state.dir = "$STATE_DIR"

[[accounts]]
name = "go-primary"
provider = "opencode"
credential = { kind = "env", name = "OPENCODE_SESSION_COOKIE" }
opencode_workspace = "wrk_2345ABCDEFGHJKLMNOPQRSTuvwx"
CFG_EOF

    # Two bodies built from the committed fixture. The stable one states a
    # reset instant on all three windows, because a window the provider leaves
    # without one is not-started and has no boundary to move; the committed
    # fixture carries the real response's own mixture, which the unit cases
    # cover and this one cannot use. The moved one pushes every instant far
    # out, with the quota counts untouched, so the only thing that changed is
    # the boundary.
    python3 - "$REPO_ROOT/tests/fixtures/meter/opencode/valid.json" \
        "$STATE_DIR/stable.json" "$STATE_DIR/moved.json" <<'PY_EOF'
import datetime
import json
import sys

source, stable_path, moved_path = sys.argv[1], sys.argv[2], sys.argv[3]
document = json.load(open(source))
meters = document["access"]["meters"]

# The instants are generated from the run's own clock rather than pinned, for
# the same reason the page revision's fixture stated durations rather than
# dates: a five-hour window whose boundary sits a year out, or one already
# elapsed, is not the state this case is about, and either would exercise a
# different branch of the classifier. The offsets are the nominal lengths.
now = datetime.datetime.now(datetime.timezone.utc)


def stamp(offset):
    return (now + offset).strftime("%Y-%m-%dT%H:%M:%S.000Z")


stable_resets = {
    "fiveHour": stamp(datetime.timedelta(hours=5)),
    "week": stamp(datetime.timedelta(days=6, hours=8)),
    "month": stamp(datetime.timedelta(days=27, hours=8)),
}
# Years out: past the declared precision plus any observation gap, by a margin
# no tolerance this system declares could absorb.
moved_resets = {
    "fiveHour": stamp(datetime.timedelta(days=1000, hours=5)),
    "week": stamp(datetime.timedelta(days=1006, hours=8)),
    "month": stamp(datetime.timedelta(days=1027, hours=8)),
}
for name, meter in meters.items():
    meter["resetsAt"] = stable_resets[name]
json.dump(document, open(stable_path, "w"))
for name, meter in meters.items():
    meter["resetsAt"] = moved_resets[name]
json.dump(document, open(moved_path, "w"))
PY_EOF

    # Two stub transports: the stable one answers every request with the
    # stable body, the moved one with the shifted body.
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
" "$STATE_DIR/stable-port.txt" "$STATE_DIR/stable.json" &
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
" "$STATE_DIR/moved-port.txt" "$STATE_DIR/moved.json" &
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
    # The ticks are back to back on purpose. The three-second spacing this
    # case used to carry existed to put each pair's re-derived drift past the
    # fixed provider-jitter envelope; the endpoint states an absolute instant,
    # so consecutive samples of an unchanging response state the identical
    # boundary at any spacing, and the nine seconds bought nothing.
    local tick
    for tick in 01 02 03; do
        step "tick-$tick-opencode-sample" env \
            "HOME=$STATE_DIR/home" \
            "AUB_STATE_DIR=$STATE_DIR" \
            "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
            "AUB_OPENCODE_ENDPOINT=http://127.0.0.1:$STABLE_PORT" \
            "OPENCODE_SESSION_COOKIE=$OPENCODE_COOKIE_MATERIAL" \
            "$AUB_BIN" sample --account go-primary
    done

    # The ledger must hold zero window anomalies after the three ticks.
    step "query-anomaly-count-after-ticks" sqlite3 "$LEDGER_DB" \
        "SELECT 'anomaly_count=' || count(*) FROM meter_window_anomaly"

    # One sample against the body whose stated resets moved years out: past
    # the declared precision plus the gap for every window, with the quota
    # counts unchanged, so each of the three windows records the typed
    # unexpected-reset-change anomaly.
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

    # Steps 1 through 3 are the three ticks, in order.
    local n
    for n in 1 2 3; do
        assert_exit 0 "$n"
        assert_stdout_contains "$n" "sample: account=go-primary outcome=success"
    done

    # Step 4: three ticks of an unchanging response recorded no anomaly of
    # any kind.
    assert_exit 0 4
    assert_stdout_contains 4 "anomaly_count=0"

    # Step 5 ran, and step 6 shows the real move still records one anomaly
    # per window, named by kind.
    assert_exit 0 5
    assert_exit 0 6
    assert_stdout_contains 6 "unexpected_reset_change=3"
}