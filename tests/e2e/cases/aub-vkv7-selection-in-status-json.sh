# aub-vkv7: `aub status --format json` carries a per-account selection
# signal. One account holds a window with real usage and a future reset, the
# other holds an untriggered window (no reset, no usage), so the two accounts
# exercise different runways while every account must carry a
# `selection.runway` in the four-value set. The scalar travels only when a
# window is measurable; the runway and confidence travel always.

CASE_ID="aub-vkv7-selection-in-status-json"
CASE_DESCRIPTION="status json carries a per-account selection block with runway in the four-value set."

CONFIG_FILE=""

case_preconditions() {
    require_command "$AUB_BIN"
    require_command jq
    require_command python3

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

    # work-primary: 18% used with 30% of its 10 h cycle left (a measurable
    # shape; with no transcript history the burn is unknown, so the runway
    # reads unknown while still carrying the block). research: an
    # untriggered window, fully available, so the runway reaches the reset.
    local now received reset_at
    now="$(date +%s%N)"
    received="$((now - 41 * 1000000000))"
    reset_at="$((now + 3 * 3600 * 1000000000))"
    printf '%s\n' "$now" > "$CASE_LOG_DIR/seeded-at-nanos.txt"

    {
        printf '{"schema_version":2,"ledger_generation":12,"accounts":['
        printf '{"account_id":1,"logical_name":"work-primary","provider":"provider-a","last_successful_observation":{"observation_id":7,"provider_observed_at_nanos":%s,"received_at_nanos":%s,"measurement_basis":"provider_observed","provider_contract_id":"contract-v1","windows":[{"semantic_key":"five_hour","scope_kind":"account_wide","scoped_model":null,"quota_used_ppm":180000,"reported_resolution_ppm":10000,"quantization":"exact","resets_at_nanos":%s,"nominal_duration_nanos":36000000000000,"is_active":true,"severity":"unknown"}]},"latest_attempt":{"attempt_id":9,"request_started_at_nanos":%s,"credential_context_id":"ctx","result":{"completed_at_nanos":%s,"outcome":"success","failure_class":null}}}' \
            "$received" "$received" "$reset_at" "$received" "$received"
        printf ','
        printf '{"account_id":2,"logical_name":"research","provider":"provider-a","last_successful_observation":{"observation_id":8,"provider_observed_at_nanos":%s,"received_at_nanos":%s,"measurement_basis":"provider_observed","provider_contract_id":"contract-v1","windows":[{"semantic_key":"seven_day","scope_kind":"account_wide","scoped_model":null,"quota_used_ppm":0,"reported_resolution_ppm":10000,"quantization":"exact","resets_at_nanos":null,"nominal_duration_nanos":604800000000000,"is_active":true,"severity":"unknown"}]},"latest_attempt":{"attempt_id":10,"request_started_at_nanos":%s,"credential_context_id":"ctx","result":{"completed_at_nanos":%s,"outcome":"success","failure_class":null}}}' \
            "$received" "$received" "$received" "$received"
        printf ']}'
    } > "$STATE_DIR/state/projection"
}

case_steps() {
    step "status json" \
        env "HOME=$STATE_DIR/home" "AUB_CONFIG_FILE=$CONFIG_FILE" "$AUB_BIN" status --format json
}

case_assertions() {
    assert_exit 0 1
    assert_json_field 1 command status
    assert_json_field 1 schema 5

    # Every account carries a selection block whose runway is one of the
    # four documented values, and whose confidence is one of the two.
    assert_selection_runways 1
    assert_selection_confidences 1

    # The untriggered account reaches its reset with no scalar: nothing to
    # average, so the scalar is absent rather than zero. The measured
    # account publishes its scalar beside the runway.
    assert_selection_absent_scalar 1 research
    assert_selection_has_scalar 1 work-primary
}

# assert_selection_runways STEP: every accounts[].selection.runway is one of
# through_reset, projected_exhaustion, exhausted_now, unknown.
assert_selection_runways() {
    local step="$1" bad total
    total="$(jq -r '.accounts | length' "$(step_dir "$step")/stdout.bin" 2>/dev/null)"
    bad="$(jq -r '[.accounts[] | select((.selection // {}) .runway as $r | ["through_reset","projected_exhaustion","exhausted_now","unknown"] | index($r) | not)] | length' "$(step_dir "$step")/stdout.bin" 2>/dev/null)"
    if [ -n "$total" ] && [ "$total" -gt 0 ] 2>/dev/null && [ "$bad" = "0" ]; then
        record_assertion "assert_selection_runways step $step" "all $total runways in set" "all $total runways in set" "pass"
    else
        record_assertion "assert_selection_runways step $step" "all runways in set" "total=${total:-?} bad=${bad:-?}" "fail"
        CASE_FAILED=1
    fi
}

# assert_selection_confidences STEP: every accounts[].selection.confidence is
# early or established.
assert_selection_confidences() {
    local step="$1" bad total
    total="$(jq -r '.accounts | length' "$(step_dir "$step")/stdout.bin" 2>/dev/null)"
    bad="$(jq -r '[.accounts[] | select((.selection // {}) .confidence as $c | ["early","established"] | index($c) | not)] | length' "$(step_dir "$step")/stdout.bin" 2>/dev/null)"
    if [ -n "$total" ] && [ "$total" -gt 0 ] 2>/dev/null && [ "$bad" = "0" ]; then
        record_assertion "assert_selection_confidences step $step" "all $total confidences in set" "all $total confidences in set" "pass"
    else
        record_assertion "assert_selection_confidences step $step" "all confidences in set" "total=${total:-?} bad=${bad:-?}" "fail"
        CASE_FAILED=1
    fi
}

# assert_selection_absent_scalar STEP ACCOUNT: the account's selection block
# exists and carries no spend_priority key, so no consumer ranks on a
# placeholder.
assert_selection_absent_scalar() {
    local step="$1" account="$2"
    if jq -e --arg account "$account" '.accounts[] | select(.account == $account) | .selection | has("runway")' "$(step_dir "$step")/stdout.bin" >/dev/null 2>&1 \
        && ! jq -e --arg account "$account" '.accounts[] | select(.account == $account) | .selection | has("spend_priority")' "$(step_dir "$step")/stdout.bin" >/dev/null 2>&1; then
        record_assertion "assert_selection_absent_scalar $account step $step" "absent" "absent" "pass"
    else
        record_assertion "assert_selection_absent_scalar $account step $step" "absent" "present-or-missing-block" "fail"
        CASE_FAILED=1
    fi
}

# assert_selection_has_scalar STEP ACCOUNT: the measured account's selection
# block carries a finite spend_priority number beside the runway.
assert_selection_has_scalar() {
    local step="$1" account="$2" value
    value="$(jq -r --arg account "$account" '.accounts[] | select(.account == $account) | .selection.spend_priority' "$(step_dir "$step")/stdout.bin" 2>/dev/null)"
    if [ -n "$value" ] && [ "$value" != "null" ] && python3 -c "import math,sys; v=float('$value'); sys.exit(0 if math.isfinite(v) else 1)" 2>/dev/null; then
        record_assertion "assert_selection_has_scalar $account step $step" "finite" "$value" "pass"
    else
        record_assertion "assert_selection_has_scalar $account step $step" "finite" "${value:-missing}" "fail"
        CASE_FAILED=1
    fi
}
