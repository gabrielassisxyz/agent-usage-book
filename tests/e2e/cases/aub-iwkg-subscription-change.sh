# aub-iwkg: the subscription behind a credential path changed. The sampler
# refuses the intruding reading instead of attributing it, records the typed
# change, and `aub doctor` surfaces the condition while `aub status` stops
# presenting the account as ordinarily fresh.
#
# Runs against the release binary with no network: the Anthropic reading
# comes through the status-line record the `aub statusline` tee writes, so
# the sampler measures end to end without a stub usage endpoint.

CASE_ID="aub-iwkg-subscription-change"
CASE_DESCRIPTION="a changed subscription behind one credential path is refused and recorded instead of attributed, and aub doctor surfaces it."

CRED_FILE=""
CONFIG_FILE=""
LEDGER_DB=""

case_preconditions() {
    require_command "$AUB_BIN"
    require_command sqlite3

    CRED_FILE="$STATE_DIR/creds/credentials.json"
    CONFIG_FILE="$STATE_DIR/aub.toml"
    LEDGER_DB="$STATE_DIR/ledger.db"

    mkdir -p "$STATE_DIR/home" "$STATE_DIR/creds"

    # The Max subscription behind the path: a fresh token, so no refresh
    # runs before sampling.
    cat > "$CRED_FILE" <<'CRED_EOF'
{"claudeAiOauth":{"accessToken":"e2e-access-max","refreshToken":"e2e-refresh-max","expiresAt":4102444800000,"scopes":["user:inference"],"subscriptionType":"max","rateLimitTier":"default_claude_max_20x"}}
CRED_EOF

    cat > "$CONFIG_FILE" <<CFG_EOF
state.dir = "$STATE_DIR"

[[accounts]]
name = "work-a"
provider = "anthropic"
credential = { kind = "file", path = "$CRED_FILE" }
CFG_EOF

    # The 2026-09-05 shape, loosely: first a Max reading, then a Pro one.
    cat > "$STATE_DIR/payload-max.json" <<'PAYLOAD_EOF'
{"session_id":"8cd9c60a-e10a-4d44-857e-6b2b931b4d9d","cwd":"/tmp/worktree/project","cost":{"total_cost_usd":1.5},"rate_limits":{"five_hour":{"used_percentage":62,"resets_at":"1786834200"},"seven_day":{"used_percentage":5,"resets_at":"1786920000"}}}
PAYLOAD_EOF
    cat > "$STATE_DIR/payload-pro.json" <<'PAYLOAD_EOF'
{"session_id":"8cd9c60a-e10a-4d44-857e-6b2b931b4d9d","cwd":"/tmp/worktree/project","cost":{"total_cost_usd":1.5},"rate_limits":{"five_hour":{"used_percentage":3,"resets_at":"1786834200"},"seven_day":{"used_percentage":14,"resets_at":"1786920000"}}}
PAYLOAD_EOF
}

sample_step() {
    step "sample $1" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "$AUB_BIN" sample
}

statusline_step() {
    step "statusline $1" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "SHALLOW_PROFILE=work-a" \
        sh -c 'cat "$1" | "$2" statusline' _ "$STATE_DIR/payload-$1.json" "$AUB_BIN"
}

case_steps() {
    # 1. The Max render lands in the status-line record.
    statusline_step "max"

    # 2. Sampled and stored under work-a; the subscription establishes.
    sample_step "max"

    # 3. The same path now holds another subscription's credential.
    step "swap credential" sh -c 'cat > "$1" <<CRED_EOF
{"claudeAiOauth":{"accessToken":"e2e-access-pro","refreshToken":"e2e-refresh-pro","expiresAt":4102444800000,"scopes":["user:inference"],"subscriptionType":"pro","rateLimitTier":"default_claude_pro"}}
CRED_EOF' _ "$CRED_FILE"

    # 4. The Pro render lands as the record's new last line.
    statusline_step "pro"

    # 5. Sampled and refused: no observation, one typed change.
    sample_step "pro"

    # 6. Doctor surfaces the condition.
    step "doctor" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "$AUB_BIN" doctor

    # 7. Status no longer presents the account as ordinarily fresh.
    step "status" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "$AUB_BIN" status

    # 8. The typed history and the single stored observation, read directly.
    step "history rows" sqlite3 "$LEDGER_DB" \
        "SELECT kind || ':' || current_identity FROM meter_subscription_change ORDER BY id;"
    step "observation count" sqlite3 "$LEDGER_DB" \
        "SELECT COUNT(*) FROM meter_observation;"
}

case_assertions() {
    assert_exit 0 1
    assert_exit 0 2
    assert_stdout_contains 2 "sample: account=work-a outcome=success"
    assert_exit 0 3
    assert_exit 0 4
    assert_exit 0 5
    assert_stdout_contains 5 "sample: account=work-a outcome=unreachable"
    assert_exit 0 6
    assert_stdout_contains 6 "[FAIL] subscription-identity-change"
    assert_stdout_contains 6 "work-a"
    assert_exit 0 7
    assert_stdout_contains 7 "stale"
    assert_stdout_contains 7 "credential changed"
    assert_exit 0 8
    assert_stdout_contains 8 "established:anthropic:max:default_claude_max_20x"
    assert_stdout_contains 8 "changed:anthropic:pro:default_claude_pro"
    assert_exit 0 9
    assert_stdout_contains 9 "1"
}
