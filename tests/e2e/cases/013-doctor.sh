# `aub doctor` and `aub doctor --fix` over a deliberately degraded state directory:
# degradations are reported, --fix performs only safe repairs, and irreparable
# conditions remain reported as failures.

CASE_ID="013-doctor"
CASE_DESCRIPTION="aub doctor reports degradations and aub doctor --fix repairs only safe ones."

CONFIG_FILE=""

case_preconditions() {
    CONFIG_FILE="$STATE_DIR/aub.toml"
    cat > "$CONFIG_FILE" <<EOT
[state]
dir = "$STATE_DIR/state"

[[transcripts]]
name = "claude-code"
root = "$STATE_DIR/transcripts/claude-code"
pattern = "**/*.jsonl"
format = "claude-code"
EOT
    mkdir -p "$STATE_DIR/state"

    # Initialize the ledger database schema by running rebuild transcripts.
    env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG_FILE" "$AUB_BIN" rebuild transcripts >/dev/null 2>&1 || true

    # Degradation 1 (repairable): undrained pending evidence in the spool.
    mkdir -p "$STATE_DIR/state/pending"
    cat > "$STATE_DIR/state/pending/attempt-99.json" <<'JSON'
{"account_id":1,"run_id":1,"due_at":1000,"due_reason":"ordinary_cadence","started_at":1000,"status_code":200,"headers":[],"body":"{}","raw_evidence":null}
JSON

    # Degradation 2 (repairable): an outdated projection with mismatched generation.
    cat > "$STATE_DIR/state/projection" <<'JSON'
{"schema_version":1,"ledger_generation":99999}
JSON

    # Degradation 3 (unrepairable by --fix): configured transcript root does not exist.
    # ($STATE_DIR/transcripts/claude-code is deliberately not created).
}

case_steps() {
    # Step 1: Pre-fix doctor in text format.
    step "pre-fix doctor text" env \
        "HOME=$STATE_DIR/home" \
        "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "$AUB_BIN" doctor

    # Step 2: Pre-fix doctor in JSON format.
    step "pre-fix doctor json" env \
        "HOME=$STATE_DIR/home" \
        "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "$AUB_BIN" doctor --format json

    # Step 3: Run doctor --fix to perform permitted repairs.
    step "doctor fix text" env \
        "HOME=$STATE_DIR/home" \
        "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "$AUB_BIN" doctor --fix

    # Step 4: Post-fix doctor in text format.
    step "post-fix doctor text" env \
        "HOME=$STATE_DIR/home" \
        "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "$AUB_BIN" doctor

    # Step 5: Post-fix doctor in JSON format.
    step "post-fix doctor json" env \
        "HOME=$STATE_DIR/home" \
        "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "$AUB_BIN" doctor --format json
}

case_assertions() {
    # Step 1: Pre-fix text shows the failures and repairable markers.
    assert_exit 0 1
    assert_stdout_contains 1 "[FAIL] pending-evidence"
    assert_stdout_contains 1 "[FAIL] projection-versus-database-generation"
    assert_stdout_contains 1 "[FAIL] transcript-roots"
    assert_stdout_contains 1 "[repairable with --fix]"

    # Step 2: Pre-fix JSON reports the fail statuses.
    assert_exit 0 2
    assert_json_field 2 "command" "doctor"
    assert_json_field 2 "schema" "1"
    assert_stdout_contains 2 '"name":"pending-evidence","status":"fail"'
    assert_stdout_contains 2 '"name":"projection-versus-database-generation","status":"fail"'
    assert_stdout_contains 2 '"name":"transcript-roots","status":"fail"'

    # Step 3: Doctor --fix performs the four permitted actions.
    assert_exit 0 3
    assert_stdout_contains 3 "Fix: 4 action(s) performed"
    assert_stdout_contains 3 "clear-expired-leases"
    assert_stdout_contains 3 "drain-pending-evidence"
    assert_stdout_contains 3 "rebuild-projection"
    assert_stdout_contains 3 "recreate-transcript-materializations"

    # Step 4: Post-fix text shows repaired checks passing, unrepairable check still failing.
    assert_exit 0 4
    assert_stdout_contains 4 "[PASS] pending-evidence"
    assert_stdout_contains 4 "[PASS] projection-versus-database-generation"
    assert_stdout_contains 4 "[FAIL] transcript-roots"

    # Step 5: Post-fix JSON reflects the repaired state.
    assert_exit 0 5
    assert_stdout_contains 5 '"name":"pending-evidence","status":"pass"'
    assert_stdout_contains 5 '"name":"projection-versus-database-generation","status":"pass"'
    assert_stdout_contains 5 '"name":"transcript-roots","status":"fail"'
}
