# aub-n27.3: records the release-binary status latency benchmark's evidence
# through the E2E runner, the same way case 015 records aub-c5m's projection
# comparison. This case captures the artifact; the numeric budget itself is
# enforced separately by bin/checks/85-status-latency-budget, which runs the
# same entry point with `--release` as its own CI gate.

CASE_ID="022-status-latency-benchmark"
CASE_DESCRIPTION="aub status, spawned 1000 times per contention case against the release binary, stays within its accepted p99 budget and the artifact is retained."

BENCHMARK_OUTPUT=""

case_preconditions() {
    BENCHMARK_OUTPUT="$STATE_DIR/status-latency-benchmark.json"
    require_command cargo
}

case_steps() {
    step --timeout 120 "emit status latency benchmark" \
        env "AUB_STATUS_BENCHMARK_OUTPUT=$BENCHMARK_OUTPUT" \
        cargo test --release --test status_benchmark emit_status_benchmark_json -- --exact --ignored --nocapture
    step "record benchmark artifact" cat "$BENCHMARK_OUTPUT"
}

case_assertions() {
    assert_exit 0 1
    assert_stdout_contains 1 '"schema": "aub.status_benchmark.v1"'
    assert_exit 0 2
    assert_stdout_contains 2 '"case": "uncontended"'
    assert_stdout_contains 2 '"case": "large_populated"'
    assert_stdout_contains 2 '"case": "active_writer"'
    assert_stdout_contains 2 '"case": "active_migration"'
    assert_stdout_contains 2 '"status_p99_budget_ns": 15000000'
}
