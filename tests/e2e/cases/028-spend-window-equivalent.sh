# aub-c0b.14: `aub spend --window-equivalent` keeps token and credit evidence while
# adding a calibration-witnessed percentage-point interval per compatible account.
#
# The first calibration is deliberately seeded before the cost model. The later
# spend request finds that immutable result but refuses it by billing semantics and
# cost-model identity, proving that a semantic mismatch is named rather than treated
# as a usable coefficient. The five-hour calibration is then superseded append-only;
# the final spend reads the successor without a source or configuration edit.

CASE_ID="028-spend-window-equivalent"
CASE_DESCRIPTION="aub spend exposes calibrated window-equivalent intervals, refuses missing or mismatched witnesses, and follows append-only supersession."

CONFIG_FILE=""
LEDGER_DB=""

case_preconditions() {
    require_command "$AUB_BIN"
    require_command sqlite3

    CONFIG_FILE="$STATE_DIR/aub.toml"
    LEDGER_DB="$STATE_DIR/ledger.db"
    mkdir -p "$STATE_DIR/home" "$STATE_DIR/transcripts/claude-code/project-window"

    cat > "$STATE_DIR/transcripts/claude-code/project-window/sessions.jsonl" <<'JSONL'
{"type":"assistant","timestamp":"2026-08-25T10:00:00.000Z","sessionId":"s-window-work","message":{"id":"m-window-work","model":"claude-3-5-sonnet","usage":{"input_tokens":1000000,"output_tokens":0,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}
{"type":"assistant","timestamp":"2026-08-25T11:00:00.000Z","sessionId":"s-window-research","message":{"id":"m-window-research","model":"claude-3-5-sonnet","usage":{"input_tokens":0,"output_tokens":1000000,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}
{"type":"assistant","timestamp":"2026-08-25T12:00:00.000Z","sessionId":"s-window-unknown","message":{"id":"m-window-unknown","model":"claude-3-5-sonnet","usage":{"input_tokens":500000,"output_tokens":0,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}
JSONL

    cat > "$CONFIG_FILE" <<CFG_EOF
state.dir = "$STATE_DIR"

[[transcripts]]
name = "claude-code"
root = "$STATE_DIR/transcripts/claude-code"
pattern = "**/*.jsonl"
format = "claude-code"
CFG_EOF
}

case_steps() {
    step "ingest" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "$AUB_BIN" ingest transcripts

    # The two markers make the first two sessions independently attributable; the
    # third intentionally remains in the unknown-account bucket.
    step "seed-account-markers" sqlite3 "$LEDGER_DB" "
        INSERT INTO session_account_marker
            (session_source, session_native, observed_at, source_ordering_key,
             logical_account, resolved_account_id, marker_source, run_source,
             run_native, evidence_designation)
        VALUES
            ('claude-code', 's-window-work', 1787616000000000000, NULL, 'work', NULL, 'hook', NULL, NULL, 'launcher_or_hook'),
            ('claude-code', 's-window-research', 1787616000000000000, NULL, 'research', NULL, 'hook', NULL, NULL, 'launcher_or_hook');
    "

    # This calibration has a fallback billing/model witness because no cost model
    # exists yet. It will be the semantic-mismatch fixture below.
    step "seed-mismatched-calibration" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "$AUB_BIN" __calibration-fixture seven_day 100

    step "seed-cost-model" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "$AUB_BIN" __cost-model-fixture complete

    step "seed-five-hour-calibration" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "$AUB_BIN" __calibration-fixture five_hour 100

    step "spend-window-equivalent" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "$AUB_BIN" spend --since 2026-08-25 --days 1 --group-by day --group-by account \
        --window-equivalent five_hour --refresh never

    step "spend-window-equivalent-json" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "$AUB_BIN" spend --since 2026-08-25 --days 1 --group-by day --group-by account \
        --window-equivalent five_hour --refresh never --format json --explain

    step "spend-missing-calibration" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "$AUB_BIN" spend --since 2026-08-25 --days 1 --group-by account \
        --window-equivalent thirty_day --refresh never

    step "spend-semantic-mismatch" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "$AUB_BIN" spend --since 2026-08-25 --days 1 --group-by account \
        --window-equivalent seven_day --refresh never --format json

    # Same scope, new immutable result. The fixture passes the active predecessor
    # to the real activation path, so the report must select this successor.
    step "supersede-five-hour-calibration" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "$AUB_BIN" __calibration-fixture five_hour 200

    step "spend-after-supersession" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "$AUB_BIN" spend --since 2026-08-25 --days 1 --group-by account \
        --window-equivalent five_hour --refresh never
}

case_assertions() {
    assert_exit 0 1
    assert_exit 0 2
    assert_exit 0 3
    assert_exit 0 4
    assert_exit 0 5

    # The parent day contains multiple accounts and therefore refuses a combined
    # percentage-point total; the two known account children each convert.
    assert_exit 0 6
    assert_stdout_contains 6 "converted to window-equivalent percentage points for five_hour"
    assert_stdout_contains 6 "day=2026-08-25  input 1500000 tokens"
    assert_stdout_contains 6 "day=2026-08-25 / account=work"
    assert_stdout_contains 6 "window equivalent [3.0000, 3.0000] percentage points"
    assert_stdout_contains 6 "day=2026-08-25 / account=research"
    assert_stdout_contains 6 "window equivalent [15.0000, 15.0000] percentage points"
    assert_stdout_contains 6 "account=unknown-account"
    assert_stdout_contains 6 "window equivalent unavailable: account attribution"
    assert_stdout_contains 6 "3.00 credits"

    assert_exit 0 7
    assert_json_field 7 "window_equivalent_window" "five_hour"
    assert_json_field 7 "groups[0].window_equivalent.status" "unavailable"
    assert_json_field 7 "groups[0].children[0].window_equivalent.unit" "percentage_points"
    assert_json_field 7 "groups[0].children[0].window_equivalent.calibration_id" "five_hour-fixture-calibration-1"
    assert_json_field 7 "groups[0].children[0].window_equivalent.lower" "150000"
    assert_json_field 7 "groups[0].children[0].credits.unit" "credits"
    assert_json_field 7 "groups[0].children[0].tokens.input.unit" "tokens"
    assert_stdout_contains 7 '"arithmetic":"converted from credits to percentage_points"'

    assert_exit 0 8
    assert_stdout_contains 8 "window equivalent unavailable: active calibration for provider anthropic and window thirty_day"
    assert_stdout_contains 8 "credits"
    assert_stdout_contains 8 "tokens"

    assert_exit 0 9
    assert_json_field 9 "groups[0].window_equivalent.status" "unavailable"
    assert_stdout_contains 9 "billing semantics match calibration"
    assert_stdout_contains 9 "cost model matches calibration"

    assert_exit 0 10
    assert_stdout_contains 10 "calibration five_hour-fixture-calibration-2 active for window five_hour"

    assert_exit 0 11
    assert_stdout_contains 11 "calibration five_hour-fixture-calibration-2"
    assert_stdout_contains 11 "window equivalent [7.5000, 7.5000] percentage points"
    assert_stdout_contains 11 "15.00 credits"
}
