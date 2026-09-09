# aub-yg2q: `aub status` says how old the reading it shows is, and
# `aub status --refresh` takes one forced sampling attempt per selected
# account through the same sampling path `aub now` uses, then renders the
# grid from the projection that pass published. Run against the release
# binary because the properties under test are about a process: what the
# default path does not do (sample), and what the ledger holds after a
# refresh whose attempt failed.
#
# The provider endpoint is unreachable (loopback port 9), so the refresh
# attempt records an `unreachable` result and the grid falls back to the
# last known reading, still carrying its age. The success path, the exact
# one-attempt-per-selected-account behaviour and the machine-readable JSON
# age run against the synthetic server under tests/status_refresh.rs.

CASE_ID="aub-yg2q-status-reading-age-and-refresh"
CASE_DESCRIPTION="status names the reading's age, takes no sample by default, and --refresh takes one forced attempt per selected account, falling back to the stored reading on failure."

LEDGER_DB=""

case_preconditions() {
    require_command "$AUB_BIN"
    require_command sqlite3
    require_command jq

    LEDGER_DB="$STATE_DIR/ledger.db"

    mkdir -p "$STATE_DIR/home" "$STATE_DIR/creds"
    echo '{"accessToken":"test-token"}' > "$STATE_DIR/creds/token.json"

    cat > "$STATE_DIR/aub.toml" <<CFG_EOF
state.dir = "$STATE_DIR"

[[accounts]]
name = "work"
provider = "anthropic"
credential = { kind = "file", path = "$STATE_DIR/creds/token.json" }
CFG_EOF
}

case_steps() {
    # 1. A real `rate-card import` against the shipped fixture is the cheapest
    #    way to get a migrated ledger on disk (the same way case 012 gets
    #    one): the schema exists and the attempt count starts at zero.
    step "create-ledger-schema" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "$AUB_BIN" rate-card import "$REPO_ROOT/tests/fixtures/rate-book/rates.toml"

    # 2. One successful observation received five minutes ago, seeded straight
    #    into the ledger: the projection the refresh publishes is built from
    #    here, so the stored reading survives the refresh's own publish.
    local now received
    now="$(date +%s%N)"
    received=$((now - 300 * 1000000000))

    step "seed-successful-observation" sqlite3 "$LEDGER_DB" "
        INSERT INTO account (id, logical_name, provider_key, first_observed_at, last_observed_at)
        VALUES (1, 'work', 'anthropic', $received, $received);

        INSERT INTO sample_run (id, trigger, started_at, ended_at, aub_version, configuration_fingerprint)
        VALUES (1, 'manual', $received, $received, '0.1.0', 'fp');

        INSERT INTO sampling_policy_snapshot (id, account_id, effective_at, ordinary_cadence_nanos, freshness_horizon_nanos, command_budget_nanos, retry_backoff_policy, reset_edge_policy, policy_algorithm_version)
        VALUES (1, 1, $received, 300000000000, 900000000000, 30000000000, '', '', 'v1');

        INSERT INTO meter_attempt (id, run_id, account_id, provider, request_started_at, policy_snapshot_id, due_at, due_reason, provider_contract_id, meter_semantics_id)
        VALUES (1, 1, 1, 'anthropic', $received, 1, $received, 'ordinary_cadence', 'contract-v1', 'semantics-v1');

        INSERT INTO meter_attempt_result (attempt_id, completed_at, elapsed_nanos, outcome, clock_anomaly)
        VALUES (1, $received, 50000000, 'success', 0);

        INSERT INTO meter_response_evidence (id, attempt_id, response_classification, received_at, evidence_capsule, capsule_schema_version, sanitizer_version, content_hash, capture_truncated)
        VALUES (1, 1, '200', $received, '{\"hash\":\"ev-1\"}', 'capsule-v1', 'san-v1', 'hash-1', 0);

        INSERT INTO meter_observation (id, attempt_id, evidence_id, account_id, provider, provider_observed_at, received_at, measurement_basis, observed_plan, observed_tier, adapter_version, provider_contract_id, meter_semantics_id, normalized_fingerprint)
        VALUES (1, 1, 1, 1, 'anthropic', $received, $received, 'provider_observed', 'pro-7d', 'pro-7d', 'adapter-v1', 'contract-v1', 'semantics-v1', 'fp-1');

        INSERT INTO meter_observation_preference (evidence_id, meter_semantics_id, current_observation_id)
        VALUES (1, 'semantics-v1', 1);

        INSERT INTO meter_window (id, observation_id, semantic_key, scope_kind, scoped_model, quota_used_ppm, reported_resolution_ppm, quantization, resets_at, nominal_duration_nanos)
        VALUES (1, 1, 'seven_day', 'account_wide', NULL, 100000, 10000, 'exact', $((now + 7 * 86400 * 1000000000)), 604800000000000);
    "
    echo "$now" > "$CASE_LOG_DIR/seeded-at-nanos.txt"

    # 3. --refresh forces exactly one attempt for the selected account. The
    #    attempt fails (unreachable endpoint), so the grid falls back to the
    #    stored reading, still carrying its age, and exits 0.
    step "status-refresh" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "AUB_ANTHROPIC_ENDPOINT=http://127.0.0.1:9" \
        "$AUB_BIN" status --refresh

    step "status-refresh-attempt-count" sqlite3 "$LEDGER_DB" \
        "SELECT count(*) FROM meter_attempt"

    step "status-refresh-unreachable-results" sqlite3 "$LEDGER_DB" \
        "SELECT count(*) FROM meter_attempt_result WHERE outcome = 'unreachable'"

    # 4. The default path reads the ledger and names the reading's age. No
    #    sampling attempt is taken: the count is unchanged.
    step "status-default" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "$AUB_BIN" status

    step "status-default-attempt-count" sqlite3 "$LEDGER_DB" \
        "SELECT count(*) FROM meter_attempt"

    # 5. The JSON surface carries the age machine-readably beside the
    #    freshness variant, still without sampling.
    step "status-json" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "$AUB_BIN" status --format json

    step "status-json-attempt-count" sqlite3 "$LEDGER_DB" \
        "SELECT count(*) FROM meter_attempt"
}

case_assertions() {
    # Step 1: the migrated ledger holds nothing yet.
    assert_exit 0 1

    # Step 3: the failed refresh is an answer, not an error: the stored
    # reading renders with its age, and the stale note carries the same age.
    assert_exit 0 3
    assert_stdout_contains 3 "  work  anthropic · observed 5m ago"
    assert_stdout_contains 3 "10%"
    assert_stdout_contains 3 "cached 5m ago"

    # Exactly one forced attempt was taken, and it failed.
    assert_exit 0 4
    assert_stdout_contains 4 "2"
    assert_exit 0 5
    assert_stdout_contains 5 "1"

    # Step 4: the default path sampled nothing more.
    assert_exit 0 6
    assert_stdout_contains 6 "  work  anthropic · observed 5m ago"
    assert_exit 0 7
    assert_stdout_contains 7 "2"

    # Step 5: the schema moved with the field set, and the age field is a
    # positive integer. The freshness verdict stays the stale one the failed
    # refresh produced; the age says how old the reading behind it is.
    assert_exit 0 8
    assert_json_field 8 "schema" "4"
    local freshness age_nanos
    freshness="$(jq -r '.accounts[0].freshness' "$(step_dir 8)/stdout.bin")"
    if [ "$freshness" = "stale" ]; then
        record_assertion "freshness variant is stale" "stale" "$freshness" "pass"
    else
        record_assertion "freshness variant is stale" "stale" "$freshness" "fail"
        CASE_FAILED=1
    fi
    age_nanos="$(jq -r '.accounts[0].observation_age_nanos' "$(step_dir 8)/stdout.bin")"
    if [ "$age_nanos" != "null" ] && [ "$age_nanos" -gt 0 ] 2>/dev/null; then
        record_assertion "observation_age_nanos is a positive integer" \
            "positive integer" "$age_nanos" "pass"
    else
        record_assertion "observation_age_nanos is a positive integer" \
            "positive integer" "$age_nanos" "fail"
        CASE_FAILED=1
    fi

    # The JSON path is the same read of the ledger: still no sampling.
    assert_exit 0 9
    assert_stdout_contains 9 "2"
}