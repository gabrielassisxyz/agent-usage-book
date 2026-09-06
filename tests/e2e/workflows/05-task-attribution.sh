#!/usr/bin/env bash
# Workflow 5 composes the release-binary `task ingest|report|overhead` case,
# which reconciles one claim boundary's task-attributed and before-first-claim
# totals through the real binary. The conservation invariant itself (every
# overhead class at once, not just the one bucket the case seeds) is a
# property of the segmentation module over generated corpora, so a focused
# proptest owns proving it holds for all eight overhead classes rather than
# a shell case hand-seeding eight fixtures.

set -euo pipefail

WORKFLOW_ID="5"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
E2E_RUNNER="$REPO_ROOT/tests/e2e/run.sh"
CASE_SOURCE_DIR="$REPO_ROOT/tests/e2e/cases"

die() {
    echo "workflow $WORKFLOW_ID: $*" >&2
    exit 1
}

command -v jq >/dev/null 2>&1 || die "jq is required by the E2E runner"

WORK_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/aub-workflow-5-XXXXXX")"
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

for case_name in 018-task.sh; do
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

[[ "$(jq -r '.cases | length' "$SUMMARY")" == 1 ]] || \
    die "the task-attribution workflow did not run exactly one composed case"
assert_case_passed 018-task

run_focused_test() {
    local label="$1"
    shift
    local log="$TEST_LOGS/$label.log"
    if ! cargo test "$@" -- --test-threads=1 --nocapture >"$log" 2>&1; then
        cat "$log" >&2
        die "focused proof $label failed; log: $log"
    fi
}

# `aub task overhead`'s own bucket reconciliation: the task-attributed total
# the report command computed independently equals the overhead-side total,
# and every bucket's share is reported alongside it.
run_focused_test overhead-buckets-reconcile-and-shares-sum-to-one --lib \
    task_overhead_buckets_reconcile_to_the_non_task_attributed_total_and_shares_sum_to_one

# The conservation invariant PLAN.md section 21 states (task-attributed plus
# overhead equals total canonical usage, with no remainder) holds over
# generated corpora that deliberately carry every one of the eight overhead
# classes at once: before-first-claim, between-claims, after-release,
# contended and unclaimed.
run_focused_test conservation-holds-for-every-overhead-class --lib \
    conservation_holds_over_generated_corpora_with_every_overhead_class

echo "workflow $WORKFLOW_ID passed; runner log: $RUN_DIR"
