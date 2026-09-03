# aub-ai3.5: the opt-in subscription-credit dimension on `aub spend`.
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
# The activation itself goes through `__cost-model-fixture`: no shipping command
# activates a cost model yet, and the coefficient constructor is crate-private, so
# this is the only way a release binary can be pointed at either model.

CASE_ID="017-spend-credits"
CASE_DESCRIPTION="aub spend --credits converts under the active cost model, refuses on unknown components, and follows a supersession."

CONFIG_FILE=""

case_preconditions() {
    CONFIG_FILE="$STATE_DIR/aub.toml"
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
[[transcripts]]
name = "claude-code"
root = "$corpus/claude-code"
pattern = "**/*.jsonl"
format = "claude-code"
EOT
}

case_steps() {
    step "activate the complete model" env \
        "HOME=$STATE_DIR/home" \
        "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "$AUB_BIN" __cost-model-fixture complete
    step "spend with credits" env \
        "HOME=$STATE_DIR/home" \
        "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "$AUB_BIN" spend --since 2026-08-25 --days 1 --group-by session --credits --refresh force
    step "spend with credits json" env \
        "HOME=$STATE_DIR/home" \
        "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "$AUB_BIN" spend --since 2026-08-25 --days 1 --group-by session --credits \
        --refresh never --format json
    step "supersede with the incomplete model" env \
        "HOME=$STATE_DIR/home" \
        "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "$AUB_BIN" __cost-model-fixture incomplete
    step "spend after supersession" env \
        "HOME=$STATE_DIR/home" \
        "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "$AUB_BIN" spend --since 2026-08-25 --days 1 --group-by session --credits --refresh never
    step "spend without credits" env \
        "HOME=$STATE_DIR/home" \
        "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "$AUB_BIN" spend --since 2026-08-25 --days 1 --group-by session --refresh never
    step "credits fixture rejects an unnamed variant" env \
        "HOME=$STATE_DIR/home" \
        "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "$AUB_BIN" __cost-model-fixture
}

case_assertions() {
    assert_exit 0 1
    assert_stdout_contains 1 "cost model anthropic-claude-messages-v1 active"

    # A conversion and a refusal side by side, with the tokens intact in both.
    assert_exit 0 2
    assert_stdout_contains 2 "converted to credits under cost model anthropic-claude-messages-v1"
    assert_stdout_contains 2 "session=claude-code:s-credits-clean"
    assert_stdout_contains 2 "0.65 credits (complete)"
    assert_stdout_contains 2 "session=claude-code:s-credits-unknown"
    assert_stdout_contains 2 "credits unavailable: unknown component: tool_use_tokens"
    assert_stdout_contains 2 "input 1000 tokens"

    assert_exit 0 3
    assert_json_field 3 "credit_model" "anthropic-claude-messages-v1"
    assert_json_field 3 "groups[0].credits.value" "0.65"
    assert_json_field 3 "groups[0].credits.unit" "credits"
    assert_json_field 3 "groups[0].credits.coverage" "complete"
    assert_json_field 3 "groups[1].credits.status" "unavailable"
    assert_json_field 3 "groups[1].credits.unit" "credits"
    assert_json_field 3 "groups[1].credits.missing[0]" "unknown component: tool_use_tokens"
    assert_json_field 3 "groups[0].tokens.input.value" "100000"

    # The supersession moves the report with no source or configuration edit.
    assert_exit 0 4
    assert_stdout_contains 4 "cost model anthropic-claude-messages-incomplete-v1 active"

    assert_exit 0 5
    assert_stdout_contains 5 "converted to credits under cost model anthropic-claude-messages-incomplete-v1"
    assert_stdout_contains 5 "credits unavailable: cache_write rate"
    assert_stdout_contains 5 "cache write 10000 tokens"

    # Token reporting is unchanged when nobody asks for credits.
    assert_exit 0 6
    assert_stdout_contains 6 "session=claude-code:s-credits-clean  input 100000 tokens · output 20000 tokens · cache read 50000 tokens · cache write 10000 tokens (complete)"

    # The hook names its two variants rather than defaulting to one.
    assert_exit 2 7
    assert_stderr_contains 7 "__cost-model-fixture requires complete or incomplete"
}
