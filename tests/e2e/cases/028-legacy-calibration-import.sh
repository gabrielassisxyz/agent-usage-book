# `aub import legacy-calibration` brings the legacy regression fit into history
# without letting it become authority: it requires a verified backup, reports
# the source by content digest, records the fit with its coefficient and date
# against an incomplete cost model, shows it in history but never as active,
# and refuses activation through the general completeness rule.

CASE_ID="028-legacy-calibration-import"
CASE_DESCRIPTION="Legacy regression fit imports as non-activatable calibration history on an incomplete cost model, stays visible but unusable, and refuses activation naming cache_write."

SOURCE=""
COPY_SOURCE=""
MALFORMED_SOURCE=""
ARCHIVE=""
CAL_ID="legacy-five-hour-fit"

aub_legacy_cal() {
    env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR/aub" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "$AUB_BIN" "$@"
}

case_preconditions() {
    require_command "$AUB_BIN"
    require_command sqlite3

    SOURCE="$STATE_DIR/legacy-calibration.json"
    COPY_SOURCE="$STATE_DIR/legacy-calibration-copy.json"
    MALFORMED_SOURCE="$STATE_DIR/malformed-legacy-calibration.json"
    ARCHIVE="$STATE_DIR/verified-archive"
    cat > "$STATE_DIR/aub.toml" <<EOF
state.dir = "$STATE_DIR/aub"

[[accounts]]
name = "primary"
provider = "anthropic"
EOF
    cat > "$SOURCE" <<'EOF'
{"format":"legacy-calibration-v1","calibration_id":"legacy-five-hour-fit","provider":"anthropic","plan_tier":"default","window":"five_hour","fitted_micros_per_point":420000,"fit_timestamp":"2026-07-01T00:00:00Z","provenance":{"origin":"legacy-regression-fit","note":"pre-rewrite regression"},"experiment":{"experiment_id":"legacy-fit-evidence-1","method":"ordinary-least-squares","evidence_ids":["legacy:e2e-obs-1","legacy:e2e-obs-2"]}}
EOF
    cat > "$COPY_SOURCE" <<'EOF'
{"format":"legacy-calibration-v1","calibration_id":"legacy-five-hour-fit","provider":"anthropic","plan_tier":"default","window":"five_hour","fitted_micros_per_point":420000,"fit_timestamp":"2026-07-01T00:00:00Z","provenance":{"origin":"legacy-regression-fit","note":"pre-rewrite regression"}}
EOF
    printf '%s\n' 'not-json' > "$MALFORMED_SOURCE"
}

case_steps() {
    step "initialize the ledger" aub_legacy_cal export --key run-id
    step "create a verified backup" aub_legacy_cal backup "$ARCHIVE"
    step "import the legacy fit" aub_legacy_cal import legacy-calibration --source "$SOURCE" --backup "$ARCHIVE" -v
    step "repeat the same import" aub_legacy_cal import legacy-calibration --source "$SOURCE" --backup "$ARCHIVE"
    step "quarantine a malformed source" aub_legacy_cal import legacy-calibration --source "$MALFORMED_SOURCE" --backup "$ARCHIVE"
    step "refuse a hardcoded copy without experiment evidence" aub_legacy_cal import legacy-calibration --source "$COPY_SOURCE" --backup "$ARCHIVE"
    step "refuse an unverified backup" aub_legacy_cal import legacy-calibration --source "$SOURCE" --backup "$STATE_DIR/not-an-archive"
    step "history shows the legacy record" aub_legacy_cal calibrate history
    step "show does not present it as active" aub_legacy_cal calibrate show
    step "activation is refused naming the missing token class" aub_legacy_cal calibrate activate "$CAL_ID" --training legacy:e2e-obs-1 --validation legacy:e2e-obs-2
    step "read durable import cardinalities" sqlite3 "$STATE_DIR/aub/ledger.db" "SELECT (SELECT COUNT(*) FROM window_calibration_result), (SELECT COUNT(*) FROM calibration_lifecycle), (SELECT COUNT(*) FROM cost_model WHERE cost_model_id = 'legacy-incomplete-cost-model-v1');"
}

case_assertions() {
    assert_exit 0 1
    assert_exit 0 2

    assert_exit 0 3
    assert_stdout_contains 3 "source_digest="
    assert_stdout_contains 3 "imported=1"
    assert_stdout_contains 3 "unchanged=0"
    assert_stdout_contains 3 "terminal_outcome=imported"
    assert_stderr_contains 3 "legacy_calibration_imported"
    assert_stderr_contains 3 "\"run\":\"run-"
    assert_stderr_contains 3 "\"source_digest\":"
    assert_stderr_contains 3 "\"verified_backup_id\":"
    assert_stderr_contains 3 "\"records_read\":{\"value\":1"
    assert_stderr_contains 3 "\"imported\":{\"value\":1"
    assert_stderr_contains 3 "\"unchanged\":{\"value\":0"
    assert_stderr_contains 3 "\"quarantined\":{\"value\":0"
    assert_stderr_contains 3 "\"terminal_outcome\":\"imported\""
    if grep -qF "$SOURCE" "$(step_dir 3)/stdout.txt" "$(step_dir 3)/stderr.txt"; then
        record_assertion "import output omits absolute source path" "absent" "present" "fail"
        CASE_FAILED=1
    else
        record_assertion "import output omits absolute source path" "absent" "absent" "pass"
    fi
    if grep -qF "420000" "$(step_dir 3)/stdout.txt" "$(step_dir 3)/stderr.txt"; then
        record_assertion "import output carries no raw coefficient" "absent" "present" "fail"
        CASE_FAILED=1
    else
        record_assertion "import output carries no raw coefficient" "absent" "absent" "pass"
    fi

    assert_exit 0 4
    assert_stdout_contains 4 "imported=0"
    assert_stdout_contains 4 "unchanged=1"
    assert_stdout_contains 4 "terminal_outcome=unchanged"

    assert_exit 0 5
    assert_stdout_contains 5 "records_read=1"
    assert_stdout_contains 5 "imported=0"
    assert_stdout_contains 5 "quarantined=1"
    assert_stdout_contains 5 "terminal_outcome=quarantined"

    assert_exit 8 6
    assert_stderr_contains 6 "hardcoded copy"
    assert_stderr_contains 6 "experiment evidence"

    assert_exit 5 7
    assert_stderr_contains 7 "backup"

    assert_exit 0 8
    assert_stdout_contains 8 "$CAL_ID"
    assert_stdout_contains 8 "provisional"

    assert_exit 0 9
    if grep -qF "$CAL_ID" "$(step_dir 9)/stdout.txt"; then
        record_assertion "show omits never-activated legacy history" "absent" "present" "fail"
        CASE_FAILED=1
    else
        record_assertion "show omits never-activated legacy history" "absent" "absent" "pass"
    fi

    assert_exit 7 10
    assert_stderr_contains 10 "cache_write"
    assert_stderr_contains 10 "legacy-incomplete-cost-model-v1"

    assert_exit 0 11
    assert_stdout_contains 11 "1|0|1"
}
