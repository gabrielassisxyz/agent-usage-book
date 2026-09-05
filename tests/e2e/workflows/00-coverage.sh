#!/usr/bin/env bash
# Workflow 0 composes the release-binary cases that exercise coverage and the
# durable two-stage attempt lifecycle. The focused Rust tests add the seeded
# reset-spanning history and the real synthetic-provider ordering proof.

set -euo pipefail

WORKFLOW_ID="0"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
E2E_RUNNER="$REPO_ROOT/tests/e2e/run.sh"
CASE_SOURCE_DIR="$REPO_ROOT/tests/e2e/cases"

die() {
    echo "workflow $WORKFLOW_ID: $*" >&2
    exit 1
}

command -v jq >/dev/null 2>&1 || die "jq is required by the E2E runner"

WORK_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/aub-workflow-0-XXXXXX")"
SELECTED_CASES="$WORK_ROOT/cases"
RUNS_DIR="$WORK_ROOT/runs"
TEST_LOGS="$WORK_ROOT/test-logs"
STATE_ROOT="$WORK_ROOT/state"
mkdir -p "$SELECTED_CASES" "$RUNS_DIR" "$TEST_LOGS"

cd "$REPO_ROOT"
AUB_BIN="${AUB_BIN:-$REPO_ROOT/target/release/aub}"
if [[ ! -x "$AUB_BIN" && "$AUB_BIN" == "$REPO_ROOT/target/release/aub" ]]; then
    target_dir="$(cargo metadata --format-version 1 --no-deps | jq -r '.target_directory')"
    AUB_BIN="$target_dir/release/aub"
fi
if [[ "$AUB_BIN" != /* ]]; then
    AUB_BIN="$REPO_ROOT/${AUB_BIN#./}"
fi
export AUB_BIN

for case_name in 009-attempt-crash.sh 013-coverage.sh; do
    case_file="$CASE_SOURCE_DIR/$case_name"
    [[ -f "$case_file" ]] || die "missing composed case: $case_file"
    cp "$case_file" "$SELECTED_CASES/$case_name"
done

RUNNER_LOG="$WORK_ROOT/runner.log"
if ! "$E2E_RUNNER" \
    --state-dir "$STATE_ROOT" \
    --cases-dir "$SELECTED_CASES" \
    --runs-dir "$RUNS_DIR" \
    --keep 1 >"$RUNNER_LOG" 2>&1; then
    cat "$RUNNER_LOG" >&2
    die "composed release-binary cases failed; log: $RUNNER_LOG"
fi

RUN_DIR="$(find "$RUNS_DIR" -mindepth 1 -maxdepth 1 -type d -printf '%T@ %p\n' \
    | sort -rn | head -n 1 | cut -d' ' -f2-)"
[[ -n "$RUN_DIR" ]] || die "the E2E runner produced no run directory"
SUMMARY="$RUN_DIR/summary.json"
for artifact in summary.json timeline.txt manifest.json; do
    [[ -f "$RUN_DIR/$artifact" ]] || die "runner log is missing $artifact"
done

assert_case_passed() {
    local case_id="$1"
    for artifact in case.log assertions.txt; do
        [[ -f "$RUN_DIR/cases/$case_id/$artifact" ]] || \
            die "runner log is missing $case_id/$artifact"
    done
    [[ "$(find "$RUN_DIR/cases/$case_id/steps" -mindepth 1 -maxdepth 1 -type d | wc -l)" -gt 0 ]] || \
        die "runner log is missing steps for $case_id"
    command grep -q '^verdict: pass$' "$RUN_DIR/cases/$case_id/case.log" || \
        die "case $case_id did not pass the runner verdict"
    jq -e --arg id "$case_id" '
        (.cases | map(select(.id == $id))) as $matches
        | ($matches | length == 1)
        and ($matches[0].verdict == "pass")
        and ((($matches[0].assertions // []) | length) > 0)
    ' "$SUMMARY" >/dev/null || die "case $case_id did not pass all declared assertions"
}

[[ "$(jq -r '.cases | length' "$SUMMARY")" == 2 ]] || \
    die "the coverage workflow did not run exactly two composed cases"
assert_case_passed 009-attempt-crash
assert_case_passed 013-coverage

run_focused_test() {
    local label="$1"
    shift
    local log="$TEST_LOGS/$label.log"
    if ! cargo test "$@" -- --test-threads=1 --nocapture >"$log" 2>&1; then
        cat "$log" >&2
        die "focused proof $label failed; log: $log"
    fi
}

# The coverage command suite owns the exact no-attempt, interrupted, reset-
# spanning, policy-denominator and threshold-floor assertions.
run_focused_test coverage-history --test coverage_command

# These tests are the cross-process half of the attempt-before-request proof:
# the real binary is sampled against the repository's loopback synthetic server,
# and the persisted start row survives a process killed while the request is in
# flight. The two-account sampler test checks that every sampled account reaches
# that server independently.
run_focused_test synthetic-request-order --test now_command \
    now_forces_persistence_then_agrees_with_an_immediate_status
run_focused_test persisted-attempt-before-request --test sample_exit \
    process_termination_durability_attempt_start_survives_without_result
run_focused_test every-sampled-account-reaches-server --test sampler_batch \
    named_accounts_are_isolated_from_ambient_credentials

echo "workflow $WORKFLOW_ID passed; runner log: $RUN_DIR"
