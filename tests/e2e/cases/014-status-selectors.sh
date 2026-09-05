# The status selector contract, exercised by the release binary over a seeded
# multi-account, multi-model projection: the account selector renders only the
# configured account it names, the model selector keeps the account-wide
# constraints plus the chosen model's windows and excludes unrelated
# model-scoped windows, the unknown-account condition is the typed usage exit,
# and an unknown model leaves only the account-wide windows. The runner
# records the exact argv, the binary and projection digests, the sanitized
# configuration and lossless streams per step; this case adds the selected
# scopes and the expected versus observed window comparison as its own
# artifact, and asserts one projection read, empty default stderr and the
# exit statuses.

CASE_ID="014-status-selectors"
CASE_DESCRIPTION="account and model selectors scope the status reading, never widen it."

CONFIG_FILE=""

case_preconditions() {
    CONFIG_FILE="$STATE_DIR/aub.toml"
    cat > "$CONFIG_FILE" <<EOT
state.dir = "$STATE_DIR/state"

[[accounts]]
name = "work-primary"
provider = "provider-a"

[[accounts]]
name = "research"
provider = "provider-a"
EOT
    mkdir -p "$STATE_DIR/state"

    # Two accounts, two models: every account reports an account-wide window
    # and a model-scoped window, so any leak of an unrelated model's window is
    # visible in the rendered value.
    local now received
    now="$(date +%s%N)"
    received="$((now - 41 * 1000000000))"
    printf '%s\n' "$now" > "$CASE_LOG_DIR/seeded-at-nanos.txt"

    {
        printf '{"schema_version":2,"ledger_generation":12,"accounts":['
        account_record 1 "work-primary" "$received"
        printf ','
        account_record 2 "research" "$received"
        printf ']}'
    } > "$STATE_DIR/state/projection"

    # The runner artifact that names what this case expected before any step
    # ran: the scopes each selector should include, and the windows the
    # limiting values come from.
    {
        echo "selected scopes per step:"
        echo "  1 account only        -> work-primary: account_wide + model:claude-model-x"
        echo "  2 model only          -> both accounts: account_wide + model:claude-model-x"
        echo "  3 account and model   -> research: account_wide + model:claude-model-x"
        echo "  4 unknown account     -> usage exit, no rendering"
        echo "  5 unknown model       -> both accounts: account_wide only"
        echo "expected limiting windows:"
        echo "  fresh steps           -> account-wide 5h window, 50% used, 50% left"
        echo "  model selector steps  -> model:claude-model-x 7d window, 70% used, 30% left"
    } > "$CASE_LOG_DIR/selected-scopes.txt"
}

account_record() {
    local id="$1" name="$2" received="$3"
    printf '{"account_id":%s,"logical_name":"%s","provider":"provider-a","last_successful_observation":{"observation_id":7,"provider_observed_at_nanos":%s,"received_at_nanos":%s,"measurement_basis":"provider_observed","provider_contract_id":"contract-v1","windows":[%s,%s,%s]},"latest_attempt":{"attempt_id":9,"request_started_at_nanos":%s,"credential_context_id":"ctx","result":{"completed_at_nanos":%s,"outcome":"success","failure_class":null}}}' \
        "$id" "$name" "$received" "$received" \
        '{"semantic_key":"five_hour","scope_kind":"account_wide","scoped_model":null,"quota_used_ppm":500000,"reported_resolution_ppm":10000,"quantization":"exact","resets_at_nanos":0,"nominal_duration_nanos":18000000000000,"is_active":true,"severity":"unknown"}' \
        "{\"semantic_key\":\"weekly\",\"scope_kind\":\"model_specific\",\"scoped_model\":\"claude-model-x\",\"quota_used_ppm\":700000,\"reported_resolution_ppm\":10000,\"quantization\":\"exact\",\"resets_at_nanos\":0,\"nominal_duration_nanos\":604800000000000,\"is_active\":true,\"severity\":\"unknown\"}" \
        "{\"semantic_key\":\"weekly\",\"scope_kind\":\"model_specific\",\"scoped_model\":\"claude-model-y\",\"quota_used_ppm\":950000,\"reported_resolution_ppm\":10000,\"quantization\":\"exact\",\"resets_at_nanos\":0,\"nominal_duration_nanos\":604800000000000,\"is_active\":true,\"severity\":\"unknown\"}" \
        "$received" "$received"
}

case_steps() {
    step "account only" \
        env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG_FILE" "$AUB_BIN" status --account research
    step "model only json" \
        env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG_FILE" "$AUB_BIN" status --model claude-model-x --format json
    step "account and model json" \
        env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG_FILE" "$AUB_BIN" status --account research --model claude-model-x --format json
    step "unknown account" \
        env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG_FILE" "$AUB_BIN" status --account ghost
    step "unknown model json" \
        env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG_FILE" "$AUB_BIN" status --model no-such-model --format json
    step "one projection read" \
        env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG_FILE" "AUB_LOG_LEVEL=debug" "$AUB_BIN" status --account research
}

case_assertions() {
    # Step 1: the selector renders exactly the configured account it names.
    # No model selector is given, so every window applies, and the unrelated
    # model's 95%-used weekly window is the account's most constrained fact.
    assert_exit 0 1
    assert_stdout_contains 1 "aub research 5% left · 7d"

    # Step 2: both accounts, each limited by the chosen model's weekly window;
    # the unrelated model's window is excluded by name.
    assert_exit 0 2
    assert_json_field 2 command status
    assert_scopes_include 2 work-primary "account_wide" "model:claude-model-x"
    assert_scopes_include 2 research "account_wide" "model:claude-model-x"
    assert_model_excluded 2 "claude-model-y"
    assert_selected_model 2 work-primary claude-model-x
    assert_selected_model 2 research claude-model-x
    assert_remaining 2 work-primary 300000
    assert_remaining 2 research 300000

    # Step 3: both selectors together render only the chosen account.
    assert_exit 0 3
    assert_scopes_include 3 research "account_wide" "model:claude-model-x"
    assert_account_absent 3 work-primary
    assert_model_excluded 3 "claude-model-y"

    # Step 4: the unknown-account condition is the typed usage exit and names
    # the account.
    assert_exit 2 4
    assert_stderr_contains 4 "unknown account 'ghost'"

    # Step 5: an unknown model leaves only the account-wide windows, and the
    # run still exits zero: the model namespace belongs to the projection, so
    # absence there is a displayable fact, not an argument error.
    assert_exit 0 5
    assert_scopes_include 5 work-primary "account_wide"
    assert_scopes_exclude 5 work-primary "model:"

    # Step 6: exactly one projection read and no network event, at debug level.
    assert_exit 0 6
    assert_stderr_contains 6 "projection_read"
    if [ "$(grep -c projection_read "$(step_dir 6)/stderr.bin")" -ne 1 ]; then
        record_assertion "assert one projection_read step 6" "1" "not 1" "fail"
        CASE_FAILED=1
    fi
    if grep -q request_attempted "$(step_dir 6)/stderr.bin"; then
        record_assertion "assert no network event step 6" "absent" "present" "fail"
        CASE_FAILED=1
    fi
}

# assert_selected_model STEP ACCOUNT MODEL: the account's JSON object names
# the model this case's selector chose.
assert_selected_model() {
    local step="$1" account="$2" model="$3"
    assert_json_object_field "$(step_dir "$step")/stdout.bin" \
        ".accounts[] | select(.account == \"$account\") | .selected_model" "$model" \
        "assert_selected_model $account step $step"
}

# assert_account_absent STEP ACCOUNT: the selector excluded the account, so no
# object names it.
assert_account_absent() {
    local step="$1" account="$2"
    if jq -e --arg account "$account" \
        '.accounts | map(select(.account == $account)) | length == 0' \
        "$(step_dir "$step")/stdout.bin" >/dev/null 2>&1; then
        record_assertion "assert_account_absent $account step $step" "absent" "absent" "pass"
    else
        record_assertion "assert_account_absent $account step $step" "absent" "present" "fail"
        CASE_FAILED=1
    fi
}

# assert_json_object_field FILE JQ_PATH EXPECTED NAME: a jq-path assertion the
# per-account objects need, since assert_json_field reads only top-level keys.
assert_json_object_field() {
    local file="$1" path="$2" expected="$3" name="$4"
    local observed
    observed="$(jq -r "$path" "$file" 2>/dev/null)"
    if [ "$observed" = "$expected" ]; then
        record_assertion "$name" "$expected" "$observed" "pass"
    else
        record_assertion "$name" "$expected" "$observed" "fail"
        CASE_FAILED=1
    fi
}

# assert_remaining STEP ACCOUNT PPM: the account's fresh reading's remaining
# parts-per-million, so the rendered number is pinned to the windows this case
# seeded rather than to whatever the binary computed.
assert_remaining() {
    local step="$1" account="$2" expected="$3"
    local observed
    observed="$(jq -r --arg account "$account" '.accounts[] | select(.account == $account) | .remaining.value' "$(step_dir "$step")/stdout.bin" 2>/dev/null)"
    if [ "$observed" = "$expected" ]; then
        record_assertion "assert_remaining $account step $step" "$expected" "$observed" "pass"
    else
        record_assertion "assert_remaining $account step $step" "$expected" "$observed" "fail"
        CASE_FAILED=1
    fi
}

# assert_scopes_include STEP ACCOUNT SCOPE...: the account's JSON object names
# every listed scope in included_scopes.
assert_scopes_include() {
    local step="$1" account="$2"
    shift 2
    local scopes
    scopes="$(jq -r --arg account "$account" '.accounts[] | select(.account == $account) | (.included_scopes // []) | join(",")' "$(step_dir "$step")/stdout.bin" 2>/dev/null)"
    local expected
    expected="$(printf '%s\n' "$@" | paste -sd, -)"
    if [ "$scopes" = "$expected" ]; then
        record_assertion "assert_scopes_include $account step $step" "$expected" "$scopes" "pass"
    else
        record_assertion "assert_scopes_include $account step $step" "$expected" "$scopes" "fail"
        CASE_FAILED=1
    fi
}

# assert_scopes_exclude STEP ACCOUNT FRAGMENT: no included scope contains the
# fragment, so an unrelated model's scope cannot leak into the document.
assert_scopes_exclude() {
    local step="$1" account="$2" fragment="$3"
    if jq -e --arg account "$account" --arg fragment "$fragment" \
        '.accounts[] | select(.account == $account) | ((.included_scopes // []) | map(select(contains($fragment))) | length == 0)' \
        "$(step_dir "$step")/stdout.bin" >/dev/null 2>&1; then
        record_assertion "assert_scopes_exclude $account:$fragment step $step" "absent" "absent" "pass"
    else
        record_assertion "assert_scopes_exclude $account:$fragment step $step" "absent" "present" "fail"
        CASE_FAILED=1
    fi
}

# assert_model_excluded STEP MODEL: no account names the model as its own.
assert_model_excluded() {
    local step="$1" model="$2"
    if jq -e --arg model "$model" \
        '.accounts | map(select(.selected_model == $model)) | length == 0' \
        "$(step_dir "$step")/stdout.bin" >/dev/null 2>&1; then
        record_assertion "assert_model_excluded $model step $step" "excluded" "excluded" "pass"
    else
        record_assertion "assert_model_excluded $model step $step" "excluded" "present" "fail"
        CASE_FAILED=1
    fi
}
