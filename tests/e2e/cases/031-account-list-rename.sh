# aub-yi5e: `aub account list` shows every recorded account row with its
# configured/not-in-config status, and `aub account rename` rewrites a
# configured account's logical name across the account row and the three
# text-keyed side tables in one transaction, leaving evidence untouched and
# status able to render under the renamed value.

CASE_ID="031-account-list-rename"
CASE_DESCRIPTION="aub account list and rename: a configured account's logical name changes everywhere the ledger stores it as text."

CONFIG_FILE=""
LEDGER_DB=""

case_preconditions() {
    require_command "$AUB_BIN"
    require_command sqlite3

    CONFIG_FILE="$STATE_DIR/aub.toml"
    LEDGER_DB="$STATE_DIR/state/ledger.db"
    mkdir -p "$STATE_DIR/state" "$STATE_DIR/home"

    # The operator's own first step, done before this command ever runs: the
    # config already names the target of the rename, "primary".
    cat > "$CONFIG_FILE" <<EOT
state.dir = "$STATE_DIR/state"

[[accounts]]
name = "primary"
provider = "anthropic"
EOT
}

case_steps() {
    # 1. Bootstrap: migrates the ledger. Nothing has been recorded yet, so
    #    the listing says so plainly rather than printing nothing.
    step "bootstrap" env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "$AUB_BIN" account list

    # 2. Seed one account row under its retired name "max", plus one row in
    #    each of the three text-keyed tables the rename must also rewrite.
    step "seed-old-account" sqlite3 "$LEDGER_DB" "
        INSERT INTO account (logical_name, provider_key, first_observed_at, last_observed_at)
        VALUES ('max', 'anthropic', 1000000000, 2000000000);
        INSERT INTO sampling_lease (account_name, holder, acquired_at, expires_at)
        VALUES ('max', 'timer-1', 500000000, 600000000);
        INSERT INTO session_account_marker
            (session_source, session_native, observed_at, source_ordering_key,
             logical_account, resolved_account_id, marker_source, run_source,
             run_native, evidence_designation)
        VALUES ('claude-code', 'sess-1', 1500000000, NULL, 'max', 1, 'hook', NULL, NULL, 'launcher_or_hook');
        INSERT INTO account_attribution_segment
            (session_id, target_kind, logical_account, input_tokens, output_tokens,
             cache_read_tokens, cache_write_tokens, computed_at)
        VALUES ('sess-1', 'account', 'max', 10, 20, 0, 0, 1600000000);
    "

    # 3. The seeded row is not the configured name, so it lists as such.
    step "list-before-rename" env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "$AUB_BIN" account list

    # 4. The rename: the config already names "primary" (step 0 above), so
    #    this is exactly the command the operator runs next.
    step "rename" env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "$AUB_BIN" account rename anthropic max primary

    # 5. Renaming a name the configuration still holds is refused before the
    #    rename ever touches the ledger: the config was renamed to "primary"
    #    in step 4's precondition and never renamed back, so this refusal is
    #    exactly what a repeated or reordered rename would hit.
    step "rename-refused-config-still-names-old" env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "$AUB_BIN" account rename anthropic primary max

    # 6. The renamed row now lists as configured, under its new name only.
    step "list-after-rename" env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "$AUB_BIN" account list

    # 7. A projection published under the new name (what a prior sample
    #    would have written): status joins it against the configured name.
    printf '{"schema_version":2,"ledger_generation":1,"accounts":[{"account_id":1,"logical_name":"primary","provider":"anthropic","last_successful_observation":null,"latest_attempt":null}]}' \
        > "$STATE_DIR/state/projection"
    step "status-after-rename" env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG_FILE" \
        "$AUB_BIN" status --format json
}

case_assertions() {
    assert_exit 0 1
    assert_stdout_contains 1 "no accounts recorded"

    assert_exit 0 3
    assert_stdout_contains 3 "max"
    assert_stdout_contains 3 "not in config"

    assert_exit 0 4
    assert_stdout_contains 4 "max"
    assert_stdout_contains 4 "primary"

    assert_exit 2 5
    assert_stderr_contains 5 "the configuration still names 'primary'"

    assert_exit 0 6
    assert_stdout_contains 6 "primary"
    assert_stdout_contains 6 "configured"
    if grep -qF "max" "$(step_dir 6)/stdout.bin"; then
        record_assertion "the retired name is gone from the listing" "absent" "present" "fail"
        CASE_FAILED=1
    else
        record_assertion "the retired name is gone from the listing" "absent" "absent" "pass"
    fi

    assert_exit 0 7
    assert_json_field 7 command status
    assert_stdout_contains 7 "primary"

    # The three text-keyed tables carry the renamed value, and the row counts
    # this bead promises are unchanged: one row moved, none duplicated or lost.
    assert_row_count "sampling_lease" "account_name = 'primary'" 1
    assert_row_count "sampling_lease" "account_name = 'max'" 0
    assert_row_count "session_account_marker" "logical_account = 'primary'" 1
    assert_row_count "session_account_marker" "logical_account = 'max'" 0
    assert_row_count "account_attribution_segment" "logical_account = 'primary'" 1
    assert_row_count "account_attribution_segment" "logical_account = 'max'" 0
    assert_row_count "account" "provider_key = 'anthropic' AND logical_name = 'primary'" 1
    assert_row_count "account" "provider_key = 'anthropic' AND logical_name = 'max'" 0
}

# assert_row_count TABLE WHERE EXPECTED: a direct SQLite count, for the ledger
# facts no CLI output surfaces (the exact row set behind each renamed table).
assert_row_count() {
    local table="$1" where="$2" expected="$3"
    local observed
    observed="$(sqlite3 "$LEDGER_DB" "SELECT count(*) FROM $table WHERE $where")"
    if [ "$observed" = "$expected" ]; then
        record_assertion "assert_row_count $table($where)" "$expected" "$observed" "pass"
    else
        record_assertion "assert_row_count $table($where)" "$expected" "$observed" "fail"
        CASE_FAILED=1
    fi
}
