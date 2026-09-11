# aub-ai3.5 / aub-6wym: the opt-in subscription-credit dimension on `aub spend`.
#
# Two sessions in one window: one whose usage the published model prices entirely,
# and one carrying an integer usage key the adapter does not recognise, which
# survives as an unknown component. The first converts; the second refuses, because
# a provider class nobody has modelled is exactly the situation a missing term is.
#
# Then the active model is superseded by one with no cache-write term, and the
# session that converted a moment ago refuses too, and the report changed with no
# source edit and no configuration key, which is the whole point of resolving the
# model through the repository.
#
# Activation goes through the shipping command now (`aub cost-model activate`): the
# first activation uses the underscore spelling the tracker and the constructor
# names use, the idempotent repeat uses the dashed stored spelling, and both reach
# the same stored model, which is what the `already active` repeat proves.

CASE_ID="017-spend-credits"
CASE_DESCRIPTION="aub spend --credits converts under the active cost model, refuses on unknown components, and follows a supersession."

CONFIG_FILE=""
LEDGER_DB=""

case_preconditions() {
    require_command sqlite3

    CONFIG_FILE="$STATE_DIR/aub.toml"
    LEDGER_DB="$STATE_DIR/ledger.db"
    local corpus="$STATE_DIR/transcripts"
    mkdir -p "$corpus/claude-code/project-credits"

    # 100k input, 20k output, 50k cache read, 10k cache write against the published
    # model: 300000 + 300000 + 15000 + 37500 = 652500 micro-credits, rendered 0.65.
    cat > "$corpus/claude-code/project-credits/clean.jsonl" <<'JSONL'
{"type":"assistant","timestamp":"2026-08-25T10:00:00.000Z","sessionId":"s-credits-clean","message":{"id":"msg_credits_1","model":"claude-3-5-sonnet","usage":{"input_tokens":100000,"output_tokens":20000,"cache_read_input_tokens":50000,"cache_creation_input_tokens":10000}}}
JSONL

    # `tool_use_tokens` is not a kind the adapter names, so it survives as an unknown
    # component and no model can claim to have priced this session.
    cat > "$corpus/claude-code/project-credits/unknown.jsonl" <<'JSONL'
{"type":"assistant","timestamp":"2026-08-25T11:00:00.000Z","sessionId":"s-credits-unknown","message":{"id":"msg_credits_2","model":"claude-3-5-sonnet","usage":{"input_tokens":1000,"output_tokens":200,"cache_read_input_tokens":0,"cache_creation_input_tokens":0,"tool_use_tokens":77}}}
JSONL

    cat > "$CONFIG_FILE" <<EOT
state.dir = "$STATE_DIR"

[[transcripts]]
name = "claude-code"
root = "$corpus/claude-code"
pattern = "**/*.jsonl"
format = "claude-code"
EOT
}

case_steps() {
    step "list with no model active" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "$AUB_BIN" cost-model list
    step "activate the complete model" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "$AUB_BIN" cost-model activate anthropic_claude_messages_v1
    step "list after activation" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "$AUB_BIN" cost-model list
    step "re-activate the active model" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "$AUB_BIN" cost-model activate anthropic-claude-messages-v1
    step "lifecycle rows after the idempotent repeat" sqlite3 "$LEDGER_DB" \
        "SELECT count(*), group_concat(event_kind) FROM cost_model_lifecycle;"
    step "spend with credits" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "$AUB_BIN" spend --since 2026-08-25 --days 1 --group-by session --credits --refresh force
    step "spend with credits json" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "$AUB_BIN" spend --since 2026-08-25 --days 1 --group-by session --credits \
        --refresh never --format json
    step "supersede with the incomplete model" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "$AUB_BIN" cost-model activate anthropic_claude_messages_incomplete_v1
    step "lifecycle rows after the supersession" sqlite3 "$LEDGER_DB" \
        "SELECT (SELECT group_concat(event_kind) FROM cost_model_lifecycle ORDER BY event_at), (SELECT cost_model.cost_model_id FROM cost_model JOIN cost_model_lifecycle ON cost_model_lifecycle.supersedes_model_id = cost_model.id WHERE cost_model_lifecycle.event_kind = 'supersession');"
    step "spend after supersession" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "$AUB_BIN" spend --since 2026-08-25 --days 1 --group-by session --credits --refresh never
    step "spend without credits" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "$AUB_BIN" spend --since 2026-08-25 --days 1 --group-by session --refresh never
    step "activate an unknown id" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "$AUB_BIN" cost-model activate nope
}

case_assertions() {
    # Nothing is active yet: both published models print, both inactive.
    assert_exit 0 1
    assert_stdout_contains 1 "anthropic-claude-messages-v1 inactive"
    assert_stdout_contains 1 "anthropic-claude-messages-incomplete-v1 inactive"

    assert_exit 0 2
    assert_stdout_contains 2 "cost model anthropic-claude-messages-v1 active"

    assert_exit 0 3
    assert_stdout_contains 3 "anthropic-claude-messages-v1 active since "
    assert_stdout_contains 3 "anthropic-claude-messages-incomplete-v1 inactive"

    # The dashed spelling of the same model prints already active: one identity,
    # two spellings.
    assert_exit 0 4
    assert_stdout_contains 4 "cost model anthropic-claude-messages-v1 already active"

    # The idempotent repeat wrote nothing: still the one activation row.
    assert_exit 0 5
    assert_stdout_contains 5 "1|activation"

    # A conversion and a refusal side by side, with the tokens intact in both.
    assert_exit 0 6
    assert_stdout_contains 6 "converted to credits under cost model anthropic-claude-messages-v1"
    assert_stdout_contains 6 "session=claude-code:s-credits-clean"
    assert_stdout_contains 6 "0.65 credits (complete)"
    assert_stdout_contains 6 "session=claude-code:s-credits-unknown"
    assert_stdout_contains 6 "credits unavailable: unknown component: tool_use_tokens"
    assert_stdout_contains 6 "input 1000 tokens"

    assert_exit 0 7
    assert_json_field 7 "credit_model" "anthropic-claude-messages-v1"
    assert_json_field 7 "groups[0].credits.value" "0.65"
    assert_json_field 7 "groups[0].credits.unit" "credits"
    assert_json_field 7 "groups[0].credits.coverage" "complete"
    assert_json_field 7 "groups[1].credits.status" "unavailable"
    assert_json_field 7 "groups[1].credits.unit" "credits"
    assert_json_field 7 "groups[1].credits.missing[0]" "unknown component: tool_use_tokens"
    assert_json_field 7 "groups[0].tokens.input.value" "100000"

    # The supersession moves the report with no source or configuration edit.
    assert_exit 0 8
    assert_stdout_contains 8 "cost model anthropic-claude-messages-incomplete-v1 active"

    # Two rows: the original activation and a supersession naming the model it
    # displaced.
    assert_exit 0 9
    assert_stdout_contains 9 "activation,supersession|anthropic-claude-messages-v1"

    assert_exit 0 10
    assert_stdout_contains 10 "converted to credits under cost model anthropic-claude-messages-incomplete-v1"
    assert_stdout_contains 10 "credits unavailable: cache_write rate"
    assert_stdout_contains 10 "cache write 10000 tokens"

    # Token reporting is unchanged when nobody asks for credits.
    assert_exit 0 11
    assert_stdout_contains 11 "session=claude-code:s-credits-clean  input 100000 tokens · output 20000 tokens · cache read 50000 tokens · cache write 10000 tokens (complete)"

    # The command names the two published ids rather than defaulting to one.
    assert_exit 2 12
    assert_stderr_contains 12 "unknown cost model 'nope'"
    assert_stderr_contains 12 "known models: anthropic-claude-messages-v1, anthropic-claude-messages-incomplete-v1"
}