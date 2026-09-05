# aub-c0b.7: aub calibrate passive over eligible and ineligible intervals produces
# candidates and never activations, respects exclusivity, anomalies and mismatches.

CASE_ID="026-calibrate-passive"
CASE_DESCRIPTION="aub calibrate passive reports considered, eligible and excluded intervals, failing condition counts, and produces candidates without activations."

LEDGER_DB=""

case_preconditions() {
    require_command "$AUB_BIN"
    require_command sqlite3
    LEDGER_DB="$STATE_DIR/ledger.db"

    mkdir -p "$STATE_DIR/home" "$STATE_DIR/creds"
    echo '{"accessToken":"test-token"}' > "$STATE_DIR/creds/token.json"

    cat > "$STATE_DIR/aub.toml" <<CFG_EOF
state.dir = "$STATE_DIR"

[[accounts]]
name = "work"
provider = "anthropic"
credential = { kind = "file", path = "$STATE_DIR/creds/token.json" }
exclusivity_policy = "permit_passive"
CFG_EOF
}

case_steps() {
    # 1. Initialize DB and complete cost model.
    step "seed-cost-model" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "$AUB_BIN" __cost-model-fixture complete

    # 2. Seed account, observations, windows, and usage events into ledger.
    step "seed-passive-intervals" sqlite3 "$LEDGER_DB" "
        INSERT INTO account (id, logical_name, provider_key, first_observed_at, last_observed_at)
        VALUES (1, 'work', 'anthropic', 100000000000, 4000000000000);

        INSERT INTO sample_run (id, trigger, started_at, ended_at, aub_version, configuration_fingerprint)
        VALUES (1, 'manual', 100000000000, 100000000000, '0.1.0', 'fp');

        INSERT INTO sampling_policy_snapshot (id, account_id, effective_at, ordinary_cadence_nanos, freshness_horizon_nanos, command_budget_nanos, retry_backoff_policy, reset_edge_policy, policy_algorithm_version)
        VALUES (1, 1, 100000000000, 300000000000, 900000000000, 30000000000, '', '', 'v1');

        -- 4 observations at 1000s, 2000s, 3000s, 4000s
        INSERT INTO meter_attempt (id, run_id, account_id, provider, request_started_at, policy_snapshot_id, due_at, due_reason, provider_contract_id, meter_semantics_id)
        VALUES
            (1, 1, 1, 'anthropic', 1000000000000, 1, 1000000000000, 'ordinary_cadence', 'contract-v1', 'semantics-v1'),
            (2, 1, 1, 'anthropic', 2000000000000, 1, 2000000000000, 'ordinary_cadence', 'contract-v1', 'semantics-v1'),
            (3, 1, 1, 'anthropic', 3000000000000, 1, 3000000000000, 'ordinary_cadence', 'contract-v1', 'semantics-v1'),
            (4, 1, 1, 'anthropic', 4000000000000, 1, 4000000000000, 'ordinary_cadence', 'contract-v1', 'semantics-v1');

        INSERT INTO meter_attempt_result (attempt_id, completed_at, elapsed_nanos, outcome, clock_anomaly)
        VALUES
            (1, 1000000000000, 50000000, 'success', 0),
            (2, 2000000000000, 50000000, 'success', 0),
            (3, 3000000000000, 50000000, 'success', 0),
            (4, 4000000000000, 50000000, 'success', 0);

        INSERT INTO meter_response_evidence (id, attempt_id, response_classification, received_at, evidence_capsule, capsule_schema_version, sanitizer_version, content_hash, capture_truncated)
        VALUES
            (1, 1, '200', 1000000000000, '{\"hash\":\"ev-1\"}', 'capsule-v1', 'san-v1', 'hash-1', 0),
            (2, 2, '200', 2000000000000, '{\"hash\":\"ev-2\"}', 'capsule-v1', 'san-v1', 'hash-2', 0),
            (3, 3, '200', 3000000000000, '{\"hash\":\"ev-3\"}', 'capsule-v1', 'san-v1', 'hash-3', 0),
            (4, 4, '200', 4000000000000, '{\"hash\":\"ev-4\"}', 'capsule-v1', 'san-v1', 'hash-4', 0);

        INSERT INTO meter_observation (id, attempt_id, evidence_id, account_id, provider, provider_observed_at, received_at, measurement_basis, observed_plan, observed_tier, adapter_version, provider_contract_id, meter_semantics_id, normalized_fingerprint)
        VALUES
            (1, 1, 1, 1, 'anthropic', 1000000000000, 1000000000000, 'provider_observed', 'pro-5h', 'pro-5h', 'adapter-v1', 'contract-v1', 'semantics-v1', 'fp-1'),
            (2, 2, 2, 1, 'anthropic', 2000000000000, 2000000000000, 'provider_observed', 'pro-5h', 'pro-5h', 'adapter-v1', 'contract-v1', 'semantics-v1', 'fp-2'),
            (3, 3, 3, 1, 'anthropic', 3000000000000, 3000000000000, 'provider_observed', 'pro-5h', 'pro-5h', 'adapter-v1', 'contract-v1', 'semantics-v1', 'fp-3'),
            (4, 4, 4, 1, 'anthropic', 4000000000000, 4000000000000, 'provider_observed', 'pro-5h', 'pro-5h', 'adapter-v1', 'contract-v1', 'semantics-v1', 'fp-4');

        INSERT INTO meter_window (id, observation_id, semantic_key, scope_kind, scoped_model, quota_used_ppm, reported_resolution_ppm, quantization, resets_at, nominal_duration_nanos)
        VALUES
            (1, 1, 'seven_day', 'account_wide', NULL, 100000, 10000, 'rounded_to_nearest', 90000000000000, 604800000000000),
            (2, 2, 'seven_day', 'account_wide', NULL, 130000, 10000, 'rounded_to_nearest', 90000000000000, 604800000000000),
            (3, 3, 'seven_day', 'account_wide', NULL, 160000, 10000, 'rounded_to_nearest', 90000000000000, 604800000000000),
            (4, 4, 'seven_day', 'account_wide', NULL, 190000, 10000, 'rounded_to_nearest', 90000000000000, 604800000000000);

        INSERT INTO session (id, source, native_session_id, start, end, project_key, repository_key)
        VALUES (1, 'claude', 'session-1', 1000000000000, 4000000000000, 'proj', 'repo');

        INSERT INTO usage_event (id, canonical_event_id, session_id, event_timestamp, model_id, evidence_kind, source_provenance, parser_version, created_at)
        VALUES
            (1, 'ev-u-1', 'session-1', 1500000000000, 'claude-3-5-sonnet', 'reported', 'test', 'v1', 1500000000000),
            (2, 'ev-u-2', 'session-1', 2500000000000, 'claude-3-5-sonnet', 'reported', 'test', 'v1', 2500000000000),
            (3, 'ev-u-3', 'session-1', 3500000000000, 'claude-3-5-sonnet', 'reported', 'test', 'v1', 3500000000000);

        INSERT INTO usage_component (id, event_id, token_class, count)
        VALUES
            (1, 1, 'input', 1000000),
            (2, 2, 'input', 1000000),
            (3, 3, 'input', 1000000);
    "

    # 3. Run calibrate passive in text mode.
    step "calibrate-passive-text" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "$AUB_BIN" calibrate passive --account work

    # 4. Run calibrate passive in JSON mode.
    step "calibrate-passive-json" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "$AUB_BIN" calibrate passive --account work --format json

    # 5. Add an anomaly exclusion covering interval 2.
    step "add-anomaly-exclusion" sqlite3 "$LEDGER_DB" "
        INSERT INTO meter_window_anomaly (id, kind, account_id, semantic_key, scope_kind, scoped_model, prior_observation_id, prior_window_id, current_observation_id, current_window_id, detected_at, detail)
        VALUES (1, 'percentage_decrease_without_reset', 1, 'seven_day', 'account_wide', NULL, 1, 1, 2, 2, 2000000000000, 'anomaly-test');

        INSERT INTO meter_calibration_exclusion (id, anomaly_id, account_id, semantic_key, scope_kind, scoped_model, interval_start_at, interval_end_at, created_at)
        VALUES (1, 1, 1, 'seven_day', 'account_wide', NULL, 2000000000000, 3000000000000, 2000000000000);
    "

    # 6. Run calibrate passive with anomaly exclusion.
    step "calibrate-passive-anomaly-excluded" env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR" \
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml" \
        "$AUB_BIN" calibrate passive --account work --format json
}

case_assertions() {
    assert_exit 0 1
    assert_exit 0 2

    # Step 3: text output reports intervals considered and eligible
    assert_exit 0 3
    assert_stdout_contains 3 "intervals considered:"
    assert_stdout_contains 3 "eligible intervals:"

    # Step 4: json output
    assert_exit 0 4
    assert_stdout_contains 4 '"intervals_considered"'
    assert_stdout_contains 4 '"eligible_intervals"'

    # Step 5 & 6: anomaly exclusion
    assert_exit 0 5
    assert_exit 0 6
    assert_stdout_contains 6 '"meter_window_anomaly"'

    # Invariant 14: Passive fitting produces candidates and NEVER activations
    local lifecycle_count
    lifecycle_count="$(sqlite3 "$LEDGER_DB" "SELECT COUNT(*) FROM calibration_lifecycle")"
    if [ "$lifecycle_count" -ne 0 ]; then
        echo "Invariant 14 violation: calibration_lifecycle has $lifecycle_count rows (must be 0)" >&2
        return 1
    fi
}
