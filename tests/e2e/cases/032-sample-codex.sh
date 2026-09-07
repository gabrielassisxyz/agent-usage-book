# `aub sample` observes the Codex meter for provider = "codex" accounts on
# both of its sources. An account whose home owns its sessions tree reads
# the provider's own rate_limits block from the newest local rollout with no
# network call; an account whose sessions is a symlink to a shared tree
# reads the provider's usage endpoint with its own credential and never
# opens a rollout, so two accounts sharing one tree never report one
# rollout under two names. The fixture homes are built in the case's own
# state directory; the tokens they contain are fakes from the committed
# sanitized fixtures, and the case asserts no byte of them reaches the
# evidence capsules.

CASE_ID="032-sample-codex"
CASE_DESCRIPTION="aub sample reads the Codex rate limits from the newest local rollout for an owning codex account and from the usage endpoint for a symlinked codex account, recording two windows each and never persisting a credential token."

LEDGER_DB=""
FIXTURE_TOKEN=""
FIXTURE_ACCESS_TOKEN=""
FIXTURE_ACCOUNT_ID=""
ENDPOINT_PORT=""
ENDPOINT_SERVER_PID=""

case_preconditions() {
    require_command "$AUB_BIN"
    require_command sqlite3
    require_command jq
    require_command python3

    LEDGER_DB="$STATE_DIR/ledger.db"

    # A Codex home with two dated session subdirectories: the July rollout is
    # the oldest and carries a different percent, so a naive first-file read
    # would fail the window assertions below. The mtimes are pinned with
    # touch so the newest-by-mtime selection cannot depend on creation order.
    mkdir -p "$STATE_DIR/codex-home/sessions/2026/07/04" \
        "$STATE_DIR/codex-home/sessions/2026/09/05"
    cp "$REPO_ROOT/tests/fixtures/meter/codex/auth-fixture.json" \
        "$STATE_DIR/codex-home/auth.json"
    FIXTURE_TOKEN="$(jq -r '.tokens.id_token' "$REPO_ROOT/tests/fixtures/meter/codex/auth-fixture.json")"
    [ -n "$FIXTURE_TOKEN" ]
    FIXTURE_ACCESS_TOKEN="$(jq -r '.tokens.access_token' "$REPO_ROOT/tests/fixtures/meter/codex/auth-fixture.json")"
    [ -n "$FIXTURE_ACCESS_TOKEN" ]
    FIXTURE_ACCOUNT_ID="$(jq -r '.tokens.account_id' "$REPO_ROOT/tests/fixtures/meter/codex/auth-fixture.json")"
    [ -n "$FIXTURE_ACCOUNT_ID" ]

    printf '%s\n' '{"timestamp":"2026-07-04T12:00:00.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":500,"cached_input_tokens":0,"cache_write_input_tokens":0,"output_tokens":50,"reasoning_output_tokens":0,"total_tokens":550},"model_context_window":258400},"rate_limits":{"limit_id":"codex","limit_name":null,"primary":{"used_percent":5.0,"window_minutes":300,"resets_at":1783221436},"secondary":{"used_percent":1.0,"window_minutes":10080,"resets_at":1783471737},"credits":{"has_credits":false,"unlimited":false,"balance":"0"},"individual_limit":null,"spend_control_reached":null,"plan_type":"plus","rate_limit_reached_type":null}}}' \
        > "$STATE_DIR/codex-home/sessions/2026/07/04/rollout-old-session.jsonl"
    printf '%s\n' '{"timestamp":"2026-09-05T22:08:20.572Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":3000,"cached_input_tokens":0,"cache_write_input_tokens":0,"output_tokens":300,"reasoning_output_tokens":150,"total_tokens":3300},"model_context_window":258400},"rate_limits":{"limit_id":"codex","limit_name":null,"primary":{"used_percent":42.0,"window_minutes":300,"resets_at":1788650033},"secondary":{"used_percent":7.0,"window_minutes":10080,"resets_at":1789174263},"credits":{"has_credits":false,"unlimited":false,"balance":"0"},"individual_limit":null,"spend_control_reached":null,"plan_type":"plus","rate_limit_reached_type":null}}}' \
        > "$STATE_DIR/codex-home/sessions/2026/09/05/rollout-new-session.jsonl"
    touch -d "2026-07-04T12:00:00Z" "$STATE_DIR/codex-home/sessions/2026/07/04/rollout-old-session.jsonl"
    touch -d "2026-09-05T22:08:20Z" "$STATE_DIR/codex-home/sessions/2026/09/05/rollout-new-session.jsonl"

    # The shared tree both symlinked homes would read: one rollout carrying
    # 99% on both windows, so any read of it under either account name fails
    # the endpoint assertions below. The owning home above never sees it.
    mkdir -p "$STATE_DIR/shared-sessions/2026/09/05"
    printf '%s\n' '{"timestamp":"2026-09-05T22:08:20.572Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":9000,"cached_input_tokens":0,"cache_write_input_tokens":0,"output_tokens":900,"reasoning_output_tokens":0,"total_tokens":9900},"model_context_window":258400},"rate_limits":{"limit_id":"codex","limit_name":null,"primary":{"used_percent":99.0,"window_minutes":300,"resets_at":1788650033},"secondary":{"used_percent":99.0,"window_minutes":10080,"resets_at":1789174263},"credits":{"has_credits":false,"unlimited":false,"balance":"0"},"individual_limit":null,"spend_control_reached":null,"plan_type":"plus","rate_limit_reached_type":null}}}' \
        > "$STATE_DIR/shared-sessions/2026/09/05/rollout-shared.jsonl"

    # The endpoint home: its sessions entry is a symlink to the shared tree,
    # the production shape (aub-er47), with its own auth.json beside it.
    mkdir -p "$STATE_DIR/codex-shared-home"
    cp "$REPO_ROOT/tests/fixtures/meter/codex/auth-fixture.json" \
        "$STATE_DIR/codex-shared-home/auth.json"
    ln -s "$STATE_DIR/shared-sessions" "$STATE_DIR/codex-shared-home/sessions"

    cat > "$STATE_DIR/aub.toml" <<CFG_EOF
state.dir = "$STATE_DIR"

[[accounts]]
name = "codex-primary"
provider = "codex"
credential = { kind = "file", path = "$STATE_DIR/codex-home/auth.json" }
codex_home = "$STATE_DIR/codex-home"

[[accounts]]
name = "codex-shared"
provider = "codex"
credential = { kind = "file", path = "$STATE_DIR/codex-shared-home/auth.json" }
codex_home = "$STATE_DIR/codex-shared-home"
CFG_EOF

    # The synthetic usage endpoint: one local server answering every request
    # with the committed sanitized endpoint capture, so the sample workflow
    # exercises the whole endpoint path without the real provider.
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
" "$STATE_DIR/endpoint-port.txt" "$REPO_ROOT/tests/fixtures/meter/codex/wham-usage-valid.json" &
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

case_steps() {
    # 1. Forced sample of the owning codex account: measured from the local
    #    rollout, no network anywhere in this adapter.
    step "sample-codex-account" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "$AUB_BIN" sample --account codex-primary

    # 2. The observation row carries the codex provider, the rollout contract
    #    and the provider-observed measurement basis.
    step "query-observation" sqlite3 "$LEDGER_DB" \
        "SELECT provider, meter_semantics_id, measurement_basis FROM meter_observation"

    # 3. Two windows: the newest rollout's percents, scaled to ppm at the
    #    provider's integer-percent resolution, both with known resets.
    step "query-windows" sqlite3 "$LEDGER_DB" \
        "SELECT w.semantic_key, w.quota_used_ppm, w.reported_resolution_ppm, w.reset_state, w.nominal_duration_nanos FROM meter_window w JOIN meter_observation o ON o.id = w.observation_id WHERE o.provider = 'codex' ORDER BY w.semantic_key"

    # 4. The evidence capsule records the provider-observed source: the file
    #    that was read and the instant the provider wrote it.
    step "query-capsule-source" sh -c \
        'sqlite3 "$1" "SELECT evidence_capsule FROM meter_response_evidence" | grep -o "\"source\":{[^}]*}"' \
        _ "$LEDGER_DB"

    # 5. No byte of the credential token reaches the persisted evidence: the
    #    token is registered as sensitive material, and this is the belt that
    #    proves it over the real binary and the real store.
    step "query-capsule-token-absent" sh -c \
        '! printf "%s" "$1" | grep -qF "$2"' \
        _ "$(sqlite3 "$LEDGER_DB" "SELECT evidence_capsule FROM meter_response_evidence")" \
        "$FIXTURE_TOKEN"

    # 6. A second sample inside the ordinary cadence is not due.
    step "sample-again-if-due" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "$AUB_BIN" sample --account codex-primary --if-due

    # 7. Forced sample of the symlinked codex account: measured from the
    #    usage endpoint through the stub server, never from the shared
    #    tree's 99% rollout.
    step "sample-shared-codex-account" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "AUB_CODEX_ENDPOINT=http://127.0.0.1:$ENDPOINT_PORT" \
        "$AUB_BIN" sample --account codex-shared

    # 8. Two windows for the shared account: the endpoint fixture's percents
    #    (0% and 27%), not the shared rollout's 99%.
    step "query-shared-windows" sqlite3 "$LEDGER_DB" \
        "SELECT w.semantic_key, w.quota_used_ppm, w.reported_resolution_ppm, w.reset_state, w.nominal_duration_nanos FROM meter_window w JOIN meter_observation o ON o.id = w.observation_id JOIN account a ON a.id = o.account_id WHERE a.logical_name = 'codex-shared' ORDER BY w.semantic_key"

    # 9. One contract per source: the rollout contract for the owning
    #    account, the endpoint contract for the shared one.
    step "query-contracts" sqlite3 "$LEDGER_DB" \
        "SELECT a.logical_name, o.provider_contract_id FROM meter_observation o JOIN account a ON a.id = o.account_id WHERE o.provider = 'codex' ORDER BY a.logical_name"

    # 10. The shared account's capsule names the endpoint that answered.
    step "query-shared-capsule" sqlite3 "$LEDGER_DB" \
        "SELECT e.evidence_capsule FROM meter_response_evidence e JOIN meter_observation o ON o.evidence_id = e.id JOIN account a ON a.id = o.account_id WHERE a.logical_name = 'codex-shared'"

    # 11. That same capsule never names the shared rollout file: the
    #     endpoint path opened no rollout. Absence is proved the way step 5
    #     proves it, by a grep that must find nothing.
    step "query-shared-capsule-no-rollout" sh -c \
        '! sqlite3 "$1" "SELECT e.evidence_capsule FROM meter_response_evidence e JOIN meter_observation o ON o.evidence_id = e.id JOIN account a ON a.id = o.account_id WHERE a.logical_name = '"'"'codex-shared'"'"'" | grep -qF "rollout-shared"' \
        _ "$LEDGER_DB"

    # 12. The endpoint bearer token and account id left no trace in the
    #     state directory's persisted evidence.
    step "query-endpoint-token-absent" sh -c \
        '! printf "%s" "$1" | grep -qF "$2" && ! printf "%s" "$1" | grep -qF "$3"' \
        _ "$(sqlite3 "$LEDGER_DB" "SELECT evidence_capsule FROM meter_response_evidence")" \
        "$FIXTURE_ACCESS_TOKEN" \
        "$FIXTURE_ACCOUNT_ID"

    # 13. Against a refused endpoint the shared account is unreachable, not
    #     another account's block: no rollout fallback on this path.
    step "sample-shared-unreachable" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "AUB_CODEX_ENDPOINT=http://127.0.0.1:9" \
        "$AUB_BIN" sample --account codex-shared
}

case_assertions() {
    if [ -n "${ENDPOINT_SERVER_PID:-}" ]; then
        kill "$ENDPOINT_SERVER_PID" 2>/dev/null || true
        wait "$ENDPOINT_SERVER_PID" 2>/dev/null || true
    fi

    # Step 1: measured, so the command exits zero.
    assert_exit 0 1
    assert_stdout_contains 1 "sample: account=codex-primary outcome=success"

    # Step 2: the observation names the codex provider, the rollout contract
    # and the provider-observed basis.
    assert_exit 0 2
    assert_stdout_contains 2 "codex|openai-chatgpt-subscription-v1|provider_observed"

    # Step 3: the newest rollout's values, 42% and 7%.
    assert_exit 0 3
    assert_stdout_contains 3 "primary|420000|10000|known|18000000000000"
    assert_stdout_contains 3 "secondary|70000|10000|known|604800000000000"

    # Step 4: the capsule records the file that was read and the provider's
    # write instant.
    assert_exit 0 4
    assert_stdout_contains 4 "sessions/2026/09/05/rollout-new-session.jsonl"
    assert_stdout_contains 4 "mtime_nanos"

    # Step 5: the token never reached the evidence capsule.
    assert_exit 0 5

    # Step 6: the due engine holds for a codex account too.
    assert_exit 0 6
    assert_stdout_contains 6 "sample: account=codex-primary not-due"

    # Step 7: the shared account samples successfully from the endpoint.
    assert_exit 0 7
    assert_stdout_contains 7 "sample: account=codex-shared outcome=success"

    # Step 8: the endpoint fixture's values, 0% and 27%, never the shared
    # rollout's 99%.
    assert_exit 0 8
    assert_stdout_contains 8 "primary|0|10000|known|18000000000000"
    assert_stdout_contains 8 "secondary|270000|10000|known|604800000000000"

    # Step 9: each account's observation names its own source contract.
    assert_exit 0 9
    assert_stdout_contains 9 "codex-primary|openai-codex-rollout-rate-limits-v1"
    assert_stdout_contains 9 "codex-shared|openai-codex-wham-usage-v1"

    # Step 10: the shared capsule is endpoint-shaped.
    assert_exit 0 10
    assert_stdout_contains 10 "endpoint"

    # Step 11: no shared rollout file was opened on the endpoint path.
    assert_exit 0 11

    # Step 12: neither endpoint secret reached the persisted evidence.
    assert_exit 0 12

    # Step 13: unreachable, still exit zero on a forced sample, and no
    # second-guessing from the shared tree.
    assert_exit 0 13
    assert_stdout_contains 13 "sample: account=codex-shared outcome=unreachable"
}
