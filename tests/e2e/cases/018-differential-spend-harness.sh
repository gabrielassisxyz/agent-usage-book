# Differential harness e2e: runs aub spend alongside legacy spend stub across
# agreement and classified disagreement modes (aub-lqe.16).

CASE_ID="018-differential-spend-harness"
CASE_DESCRIPTION="Differential harness compares aub spend against legacy spend stub with reproducible corpus digest and classification."

CONFIG_FILE=""
STUB_BIN=""

case_preconditions() {
    CONFIG_FILE="$STATE_DIR/aub.toml"
    STUB_BIN="$REPO_ROOT/tests/fixtures/differential/legacy_spend_stub.sh"
    [ -x "$STUB_BIN" ] || die "legacy stub not executable: $STUB_BIN"

    local corpus="$REPO_ROOT/tests/fixtures/differential/small_corpus"
    cat > "$CONFIG_FILE" <<EOT
[[transcripts]]
name = "claude-code"
root = "$corpus/claude-code"
pattern = "**/*.jsonl"
format = "claude-code"
EOT
}

case_steps() {
    step "aub spend json" env \
        "HOME=$STATE_DIR/home" \
        "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "$AUB_BIN" spend --since 2026-08-25 --days 2 --group-by day --refresh force --format json

    step "legacy stub agreement" \
        "$STUB_BIN" --since 2026-08-25 --days 2 --scenario agreement

    step "legacy stub classified" \
        "$STUB_BIN" --since 2026-08-25 --days 2 --scenario classified_disagreement
}

case_assertions() {
    assert_exit 0 1
    assert_exit 0 2
    assert_exit 0 3

    assert_json_field 1 "command" "spend"
    assert_json_field 1 "schema" "1"
    assert_json_field 1 "window.since" "2026-08-25"
    assert_json_field 1 "groups[0].tokens.input.value" "1200"
    assert_json_field 1 "groups[0].tokens.output.value" "600"
    assert_json_field 1 "groups[0].tokens.cache_write.value" "3000"

    assert_json_field 2 "tool" "legacy-spend"
    assert_json_field 2 "periods[0].period" "2026-08-25"
    assert_json_field 2 "periods[0].total" "6800"

    assert_json_field 3 "tool" "legacy-spend"
    assert_json_field 3 "periods[0].period" "2026-08-25"
    assert_json_field 3 "periods[0].cache_write" "0"
}
