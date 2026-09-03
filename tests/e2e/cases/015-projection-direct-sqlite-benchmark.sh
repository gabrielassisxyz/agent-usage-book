# aub-me5.12: the direct comparator stays test-only, but this release-harness
# case records the exact command, output bytes, exit status, timing and decision
# input needed to evaluate the projection before its hardening work proceeds.

CASE_ID="015-projection-direct-sqlite-benchmark"
CASE_DESCRIPTION="projection and test-only direct SQLite status reads emit bounded, structured comparison evidence without contacting a provider."

BENCHMARK_OUTPUT=""

case_preconditions() {
    BENCHMARK_OUTPUT="$STATE_DIR/projection-direct-sqlite-benchmark.json"
    require_command cargo
}

case_steps() {
    step --timeout 30 "emit projection comparison" \
        env "AUB_PROJECTION_BENCHMARK_OUTPUT=$BENCHMARK_OUTPUT" \
        cargo test --lib projection::benchmark::tests::emit_projection_benchmark_json -- --exact --ignored --nocapture
    step "record benchmark artifact" cat "$BENCHMARK_OUTPUT"
}

case_assertions() {
    assert_exit 0 1
    assert_stdout_contains 1 '"schema": "aub.projection_benchmark.v1"'
    assert_exit 0 2
    assert_stdout_contains 2 '"case": "uncontended"'
    assert_stdout_contains 2 '"case": "large_populated"'
    assert_stdout_contains 2 '"case": "active_writer"'
    assert_stdout_contains 2 '"case": "active_migration"'
    assert_stdout_contains 2 '"value": "unmeasured"'
    assert_stdout_contains 2 '"direct_sqlite_read_only"'
}
