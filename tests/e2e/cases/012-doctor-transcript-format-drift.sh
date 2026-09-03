# `aub doctor --transcript-format-drift` over seeded corpora: matching corpus,
# uncovered field, quarantine growth and no configured roots.

CASE_ID="012-doctor-transcript-format-drift"
CASE_DESCRIPTION="aub doctor --transcript-format-drift reports format drift, quarantine and roots status."

CONFIG_MATCHING=""
CONFIG_DRIFT=""
CONFIG_EMPTY=""

case_preconditions() {
    local corpus_base="$STATE_DIR/transcripts"
    mkdir -p "$corpus_base/matching/claude-code" "$corpus_base/drift/claude-code"

    # Matching corpus: valid Claude Code records matching fixture shapes
    cat > "$corpus_base/matching/claude-code/session.jsonl" <<'JSONL'
{"type":"assistant","message":{"id":"msg_0001","model":"claude-opus-4","usage":{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":20,"cache_creation_input_tokens":10}},"timestamp":"2026-08-25T10:00:00Z","uuid":"uuid-0001"}
JSONL

    # Drift corpus: contains an uncovered field and a malformed record (quarantine growth)
    cat > "$corpus_base/drift/claude-code/session.jsonl" <<'JSONL'
{"type":"assistant","message":{"id":"msg_0001","model":"claude-opus-4","usage":{"input_tokens":100,"output_tokens":50,"unknown_feature_field":999}},"timestamp":"2026-08-25T10:00:00Z","uuid":"uuid-0001"}
{"type":"assistant","message":{"id":"msg_0002","model":"claude-opus-4","usage":{"input_tokens":"wrong-type","output_tokens":5}},"timestamp":"2026-08-25T10:00:01Z","uuid":"uuid-0002"}
JSONL

    CONFIG_MATCHING="$STATE_DIR/aub_matching.toml"
    cat > "$CONFIG_MATCHING" <<EOT
[[transcripts]]
name = "claude-code"
root = "$corpus_base/matching/claude-code"
pattern = "**/*.jsonl"
format = "claude-code"
EOT

    CONFIG_DRIFT="$STATE_DIR/aub_drift.toml"
    cat > "$CONFIG_DRIFT" <<EOT
[[transcripts]]
name = "claude-code"
root = "$corpus_base/drift/claude-code"
pattern = "**/*.jsonl"
format = "claude-code"
EOT

    CONFIG_EMPTY="$STATE_DIR/aub_empty.toml"
    cat > "$CONFIG_EMPTY" <<EOT
# empty transcripts configuration
EOT
}

case_steps() {
    # Step 1: Matching corpus (Text)
    step "matching text" env \
        "HOME=$STATE_DIR/home" \
        "AUB_CONFIG_FILE=$CONFIG_MATCHING" \
        "$AUB_BIN" doctor --transcript-format-drift
    # Step 2: Matching corpus (JSON)
    step "matching json" env \
        "HOME=$STATE_DIR/home" \
        "AUB_CONFIG_FILE=$CONFIG_MATCHING" \
        "$AUB_BIN" doctor --transcript-format-drift --format json
    # Step 3: Drift & Quarantine corpus (Text)
    step "drift text" env \
        "HOME=$STATE_DIR/home" \
        "AUB_CONFIG_FILE=$CONFIG_DRIFT" \
        "$AUB_BIN" doctor --transcript-format-drift
    # Step 4: Drift & Quarantine corpus (JSON)
    step "drift json" env \
        "HOME=$STATE_DIR/home" \
        "AUB_CONFIG_FILE=$CONFIG_DRIFT" \
        "$AUB_BIN" doctor --transcript-format-drift --format json
    # Step 5: No configured roots (Text)
    step "empty roots text" env \
        "HOME=$STATE_DIR/home" \
        "AUB_CONFIG_FILE=$CONFIG_EMPTY" \
        "$AUB_BIN" doctor --transcript-format-drift
    # Step 6: No configured roots (JSON)
    step "empty roots json" env \
        "HOME=$STATE_DIR/home" \
        "AUB_CONFIG_FILE=$CONFIG_EMPTY" \
        "$AUB_BIN" doctor --transcript-format-drift --format json
}

case_assertions() {
    # Step 1: Matching corpus text
    assert_exit 0 1
    assert_stdout_contains 1 "Doctor: Transcript Format Drift"
    assert_stdout_contains 1 "Source: claude-code"
    assert_stdout_contains 1 "Quarantined records: 0"
    assert_stdout_contains 1 "All record shapes covered by committed fixtures"

    # Step 2: Matching corpus JSON
    assert_exit 0 2
    assert_json_field 2 "command" "doctor"
    assert_json_field 2 "schema" "1"
    assert_json_field 2 "check" "transcript-format-drift"
    assert_stdout_contains 2 '"has_configured_roots":true'
    assert_stdout_contains 2 '"overall_drift_detected":false'
    assert_json_field 2 "sources[0].quarantined_records" "0"

    # Step 3: Drift corpus text
    assert_exit 0 3
    assert_stdout_contains 3 "UNCOVERED FORMAT DRIFT DETECTED"
    assert_stdout_contains 3 "message.usage.unknown_feature_field"
    assert_stdout_contains 3 "wrong_field_type: 1"
    assert_stdout_contains 3 "Next action: Capture and sanitize"

    # Step 4: Drift corpus JSON
    assert_exit 0 4
    assert_json_field 4 "command" "doctor"
    assert_json_field 4 "check" "transcript-format-drift"
    assert_stdout_contains 4 '"overall_drift_detected":true'
    assert_json_field 4 "sources[0].quarantined_records" "1"
    assert_json_field 4 "sources[0].quarantine_by_class.wrong_field_type" "1"
    assert_json_field 4 "sources[0].uncovered_fields[0]" "message.usage.unknown_feature_field"

    # Step 5: Empty roots text exits zero
    assert_exit 0 5
    assert_stdout_contains 5 "No configured transcript roots"

    # Step 6: Empty roots JSON exits zero
    assert_exit 0 6
    assert_json_field 6 "command" "doctor"
    assert_json_field 6 "check" "transcript-format-drift"
    assert_stdout_contains 6 '"has_configured_roots":false'
}
