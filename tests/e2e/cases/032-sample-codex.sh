# `aub sample` observes the Codex meter for a provider = "codex" account with
# no network call: the newest rollout under the account's codex_home carries
# the provider's own rate_limits block, the account's identity comes from the
# JWT payload in that home's auth.json, and the observation carries two
# windows. The fixture home is built in the case's own state directory; the
# token it contains is a fake from the committed sanitized fixture, and the
# case asserts no byte of it reaches the evidence capsule.

CASE_ID="032-sample-codex"
CASE_DESCRIPTION="aub sample reads the Codex rate limits from the newest local rollout for a codex account, records two windows, and never persists the credential token."

LEDGER_DB=""
FIXTURE_TOKEN=""

case_preconditions() {
    require_command "$AUB_BIN"
    require_command sqlite3
    require_command jq

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

    printf '%s\n' '{"timestamp":"2026-07-04T12:00:00.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":500,"cached_input_tokens":0,"cache_write_input_tokens":0,"output_tokens":50,"reasoning_output_tokens":0,"total_tokens":550},"model_context_window":258400},"rate_limits":{"limit_id":"codex","limit_name":null,"primary":{"used_percent":5.0,"window_minutes":300,"resets_at":1783221436},"secondary":{"used_percent":1.0,"window_minutes":10080,"resets_at":1783471737},"credits":{"has_credits":false,"unlimited":false,"balance":"0"},"individual_limit":null,"spend_control_reached":null,"plan_type":"plus","rate_limit_reached_type":null}}}' \
        > "$STATE_DIR/codex-home/sessions/2026/07/04/rollout-old-session.jsonl"
    printf '%s\n' '{"timestamp":"2026-09-05T22:08:20.572Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":3000,"cached_input_tokens":0,"cache_write_input_tokens":0,"output_tokens":300,"reasoning_output_tokens":150,"total_tokens":3300},"model_context_window":258400},"rate_limits":{"limit_id":"codex","limit_name":null,"primary":{"used_percent":42.0,"window_minutes":300,"resets_at":1788650033},"secondary":{"used_percent":7.0,"window_minutes":10080,"resets_at":1789174263},"credits":{"has_credits":false,"unlimited":false,"balance":"0"},"individual_limit":null,"spend_control_reached":null,"plan_type":"plus","rate_limit_reached_type":null}}}' \
        > "$STATE_DIR/codex-home/sessions/2026/09/05/rollout-new-session.jsonl"
    touch -d "2026-07-04T12:00:00Z" "$STATE_DIR/codex-home/sessions/2026/07/04/rollout-old-session.jsonl"
    touch -d "2026-09-05T22:08:20Z" "$STATE_DIR/codex-home/sessions/2026/09/05/rollout-new-session.jsonl"

    cat > "$STATE_DIR/aub.toml" <<CFG_EOF
state.dir = "$STATE_DIR"

[[accounts]]
name = "codex-primary"
provider = "codex"
credential = { kind = "file", path = "$STATE_DIR/codex-home/auth.json" }
codex_home = "$STATE_DIR/codex-home"
CFG_EOF
}

case_steps() {
    # 1. Forced sample of the codex account: measured from the local rollout,
    #    no network anywhere in this adapter.
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
}

case_assertions() {
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
}