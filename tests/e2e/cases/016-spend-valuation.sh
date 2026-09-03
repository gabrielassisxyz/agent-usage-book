# aub-wyu.3: opt-in valuation column on `aub spend`.
# Verifies that:
# 1. `aub spend` without `--value` is complete and omits valuation entirely with no rate-card lookup.
# 2. `aub spend --value api-list` adds the valuation column.
# 3. An aggregate missing a rate (missing cache-write price) renders the unavailable form
#    while keeping all other token counts and dimensions intact, and neither prints a monetary zero.
# 4. JSON output carries `api_list_price_equivalent` and `rate_card_version`.
# 5. `--explain` displays the exact rate-card version.

CASE_ID="016-spend-valuation"
CASE_DESCRIPTION="aub spend with and without --value api-list over seeded corpus with missing cache-write price."

CONFIG_FILE=""
RATE_FILE=""

case_preconditions() {
    CONFIG_FILE="$STATE_DIR/aub.toml"
    RATE_FILE="$STATE_DIR/rates.toml"
    local corpus="$STATE_DIR/transcripts"
    mkdir -p "$corpus/claude-code/project-a"

    cat > "$corpus/claude-code/project-a/session.jsonl" <<'JSONL'
{"type":"assistant","timestamp":"2026-08-25T10:00:00.000Z","sessionId":"s-val-1","message":{"id":"msg_val_1","model":"claude-3-5-sonnet","usage":{"input_tokens":100000,"cache_creation_input_tokens":10000,"cache_read_input_tokens":20000,"output_tokens":10000}}}
{"type":"assistant","timestamp":"2026-08-25T11:00:00.000Z","sessionId":"s-val-2","message":{"id":"msg_val_2","model":"claude-3-haiku","usage":{"input_tokens":100000,"cache_creation_input_tokens":10000,"cache_read_input_tokens":20000,"output_tokens":10000}}}
JSONL

    cat > "$CONFIG_FILE" <<EOT
[[transcripts]]
name = "claude-code"
root = "$corpus/claude-code"
pattern = "**/*.jsonl"
format = "claude-code"
EOT

    # Rate card fixture: claude-3-5-sonnet has all rates; claude-3-haiku is MISSING cache_write_5m rate
    cat > "$RATE_FILE" <<'TOML'
[publication]
source = "https://pricing.anthropic.example"
published_at = "2026-06-24T00:00:00Z"

[[card]]
vendor = "anthropic"
model = "claude-3-5-sonnet"
token_class = "input"
rate = "3.00"
currency = "USD"
billing_basis = "per_million_tokens"
effective_start = "2026-06-24"
published_at = "2026-06-24"
source = "https://pricing.anthropic.example"

[[card]]
vendor = "anthropic"
model = "claude-3-5-sonnet"
token_class = "output"
rate = "15.00"
currency = "USD"
billing_basis = "per_million_tokens"
effective_start = "2026-06-24"
published_at = "2026-06-24"
source = "https://pricing.anthropic.example"

[[card]]
vendor = "anthropic"
model = "claude-3-5-sonnet"
token_class = "cache_read"
rate = "0.30"
currency = "USD"
billing_basis = "per_million_tokens"
effective_start = "2026-06-24"
published_at = "2026-06-24"
source = "https://pricing.anthropic.example"

[[card]]
vendor = "anthropic"
model = "claude-3-5-sonnet"
token_class = "cache_write_5m"
rate = "3.75"
currency = "USD"
billing_basis = "per_million_tokens"
effective_start = "2026-06-24"
published_at = "2026-06-24"
source = "https://pricing.anthropic.example"

[[card]]
vendor = "anthropic"
model = "claude-3-haiku"
token_class = "input"
rate = "0.25"
currency = "USD"
billing_basis = "per_million_tokens"
effective_start = "2026-06-24"
published_at = "2026-06-24"
source = "https://pricing.anthropic.example"

[[card]]
vendor = "anthropic"
model = "claude-3-haiku"
token_class = "output"
rate = "1.25"
currency = "USD"
billing_basis = "per_million_tokens"
effective_start = "2026-06-24"
published_at = "2026-06-24"
source = "https://pricing.anthropic.example"

[[card]]
vendor = "anthropic"
model = "claude-3-haiku"
token_class = "cache_read"
rate = "0.03"
currency = "USD"
billing_basis = "per_million_tokens"
effective_start = "2026-06-24"
published_at = "2026-06-24"
source = "https://pricing.anthropic.example"
TOML
}

case_steps() {
    step "import rate cards" env \
        "HOME=$STATE_DIR/home" \
        "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "$AUB_BIN" rate-card import "$RATE_FILE"
    step "unvalued spend text" env \
        "HOME=$STATE_DIR/home" \
        "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "$AUB_BIN" spend --since 2026-08-25 --days 1 --group-by session --refresh force
    step "valued spend text" env \
        "HOME=$STATE_DIR/home" \
        "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "$AUB_BIN" spend --since 2026-08-25 --days 1 --group-by session --value api-list --refresh never
    step "valued spend json" env \
        "HOME=$STATE_DIR/home" \
        "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "$AUB_BIN" spend --since 2026-08-25 --days 1 --group-by session --value api-list --refresh never --format json
    step "valued spend explain" env \
        "HOME=$STATE_DIR/home" \
        "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "$AUB_BIN" spend --since 2026-08-25 --days 1 --group-by session --value api-list --explain --refresh never
}

case_assertions() {
    # 1. Rate card import succeeds
    assert_exit 0 1
    assert_stdout_contains 1 "added=7"

    # 2. Unvalued spend is complete and omits valuation
    assert_exit 0 2
    assert_stdout_contains 2 "spend from 2026-08-25 to 2026-08-26 (UTC days, end exclusive), grouped by session"
    assert_stdout_contains 2 "session=claude-code:s-val-1  input 100000 tokens · output 10000 tokens · cache read 20000 tokens · cache write 10000 tokens (complete)"
    assert_stdout_contains 2 "session=claude-code:s-val-2  input 100000 tokens · output 10000 tokens · cache read 20000 tokens · cache write 10000 tokens (complete)"

    # 3. Valued spend text adds valuation column, renders unavailable form for missing cache-write price, and neither prints $0.00
    assert_exit 0 3
    assert_stdout_contains 3 "spend from 2026-08-25 to 2026-08-26 (UTC days, end exclusive), grouped by session, valued at API list-price equivalent"
    assert_stdout_contains 3 'session=claude-code:s-val-1  input 100000 tokens · output 10000 tokens · cache read 20000 tokens · cache write 10000 tokens · API list-price equivalent $0.49 (complete)'
    assert_stdout_contains 3 "session=claude-code:s-val-2  input 100000 tokens · output 10000 tokens · cache read 20000 tokens · cache write 10000 tokens · API list-price equivalent unavailable (complete)"

    # 4. Valued spend JSON includes rate_card_version and api_list_price_equivalent
    assert_exit 0 4
    assert_json_field 4 "command" "spend"
    assert_json_field 4 "rate_card_version" "rate-card-2026-06-24"
    assert_json_field 4 "groups[0].api_list_price_equivalent.value" "0.49"
    assert_json_field 4 "groups[0].api_list_price_equivalent.unit" "usd"
    assert_json_field 4 "groups[1].api_list_price_equivalent.status" "unavailable"
    assert_json_field 4 "groups[1].api_list_price_equivalent.known_price_subtotal.value" "0.04"
    assert_json_field 4 "groups[1].api_list_price_equivalent.known_price_subtotal.unit" "usd"

    # 5. Explain output identifies exact rate card version
    assert_exit 0 5
    assert_stdout_contains 5 "rate card: rate-card-2026-06-24"
}
