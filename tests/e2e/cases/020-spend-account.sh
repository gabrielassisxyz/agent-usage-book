# `aub spend --group-by account` over a seeded corpus with a mid-session account
# switch: one claude-code session with two messages, and two account markers
# inserted between them so the earlier message belongs to account-a and the
# later one to account-b. The switch must show in the per-account totals, the
# account totals must reconcile with a plain day report, and `--explain` must
# name the markers behind each attribution.

CASE_ID="020-spend-account"
CASE_DESCRIPTION="aub spend --group-by account reflects a mid-session account switch and its totals reconcile."

CONFIG_FILE=""
LEDGER_DB=""

case_preconditions() {
    require_command "$AUB_BIN"
    require_command sqlite3

    CONFIG_FILE="$STATE_DIR/aub.toml"
    LEDGER_DB="$STATE_DIR/state/ledger.db"
    local corpus="$STATE_DIR/transcripts/claude-code/project-a"
    mkdir -p "$corpus" "$STATE_DIR/state" "$STATE_DIR/home"

    cat > "$corpus/session.jsonl" <<'JSONL'
{"type":"assistant","timestamp":"2026-08-25T10:00:00.000Z","sessionId":"s-acct-1","message":{"id":"msg_acct_1","usage":{"input_tokens":100,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":10,"service_tier":"standard"}}}
{"type":"assistant","timestamp":"2026-08-25T12:00:00.000Z","sessionId":"s-acct-1","message":{"id":"msg_acct_2","usage":{"input_tokens":400,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":40,"service_tier":"standard"}}}
JSONL

    cat > "$CONFIG_FILE" <<EOT
state.dir = "$STATE_DIR/state"

[[transcripts]]
name = "claude-code"
root = "$STATE_DIR/transcripts/claude-code"
pattern = "**/*.jsonl"
format = "claude-code"
EOT
}

case_steps() {
    # 1. Ingest the corpus into the canonical ledger.
    step "ingest" env \
        "HOME=$STATE_DIR/home" \
        "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "$AUB_BIN" spend --since 2026-08-25 --days 1 --refresh force

    # 2. Seed two account markers for the ingested session: account-a from
    #    before the first message, account-b from between the two messages. The
    #    marker join addresses the session by (source_namespace, native id)
    #    exactly as the canonical read does, so take both from the ledger.
    local ids marker_a marker_b
    ids="$(sqlite3 "$LEDGER_DB" "SELECT DISTINCT o.source_namespace || '|' || e.session_id FROM usage_occurrence o JOIN usage_event e ON e.id = o.event_id")"
    local src="${ids%%|*}" native="${ids##*|}"
    marker_a="$(date -u -d '2026-08-25T09:00:00Z' +%s%N)"
    marker_b="$(date -u -d '2026-08-25T11:00:00Z' +%s%N)"
    sqlite3 "$LEDGER_DB" <<SQL
INSERT INTO session_account_marker
    (session_source, session_native, observed_at, source_ordering_key, logical_account,
     resolved_account_id, marker_source, run_source, run_native, evidence_designation)
VALUES
    ('$src', '$native', $marker_a, NULL, 'account-a', NULL, 'hook', NULL, NULL, 'launcher_or_hook'),
    ('$src', '$native', $marker_b, NULL, 'account-b', NULL, 'hook', NULL, NULL, 'launcher_or_hook');
SQL

    # 3. Account grouping, text (step 2 in the runner's numbering: the sqlite
    #    seeding above is inline, not a step).
    step "by-account" env \
        "HOME=$STATE_DIR/home" \
        "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "$AUB_BIN" spend --since 2026-08-25 --days 1 --refresh never --group-by account

    # 4. Account grouping, JSON (step 3).
    step "by-account-json" env \
        "HOME=$STATE_DIR/home" \
        "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "$AUB_BIN" spend --since 2026-08-25 --days 1 --refresh never --group-by account --format json

    # 5. Account grouping with explain naming the marker evidence (step 4).
    step "by-account-explain" env \
        "HOME=$STATE_DIR/home" \
        "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "$AUB_BIN" spend --since 2026-08-25 --days 1 --refresh never --group-by account --explain

    # 6. Plain day grouping, to reconcile the account totals against (step 5).
    step "by-day-json" env \
        "HOME=$STATE_DIR/home" \
        "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "$AUB_BIN" spend --since 2026-08-25 --days 1 --refresh never --group-by day --format json
}

case_assertions() {
    assert_exit 0 1

    # The switch is visible: the 10:00 message (input 100) is account-a, the
    # 12:00 message (input 400) is account-b.
    assert_exit 0 2
    assert_stdout_contains 2 "grouped by account"
    assert_stdout_contains 2 "account=account-a  input 100 tokens · output 10 tokens"
    assert_stdout_contains 2 "account=account-b  input 400 tokens · output 40 tokens"

    assert_exit 0 3
    assert_json_field 3 "grouping[0]" "account"
    assert_json_field 3 "groups[0].key" "account=account-a"
    assert_json_field 3 "groups[0].tokens.input.value" "100"
    assert_json_field 3 "groups[1].key" "account=account-b"
    assert_json_field 3 "groups[1].tokens.input.value" "400"

    # explain names the markers behind each attribution, in the human output.
    assert_exit 0 4
    assert_stdout_contains 4 "account explain:"
    assert_stdout_contains 4 "account=account-a  evidence_class=explicit_launcher_or_hook"
    assert_stdout_contains 4 "account=account-b  evidence_class=explicit_launcher_or_hook"
    assert_stdout_contains 4 "session_account_marker:"

    # The account totals reconcile with the plain day report: 100 + 400 = 500.
    assert_exit 0 5
    assert_json_field 5 "groups[0].tokens.input.value" "500"
}
