#!/usr/bin/env bash
# Workflow 3 composes the release-binary `can-run` case: a real fresh sample
# against a stub provider, real persisted calibration and cost-model rows, and
# real ingested task history, joined through the shipped binary into the
# limiting-window-by-headroom advisory, an uncalibrated-window refusal and a
# stale-meter refusal.
#
# The case renames the plan's window identities (`five_hour`,
# `seven_day_sonnet`) to fit its own seeded fixtures, so it proves the
# mechanism rather than the plan's own numbers. The plan's worked example
# (PLAN.md section 51: `account:5h` at 38.0% holding less work than
# `model-x:weekly` at 52.0%) is reproduced verbatim, losslessly, by a focused
# test instead of being re-typed into a second case fixture here; composing
# it is this workflow's assumption, recorded because the bead's own wording
# ("belongs in the script verbatim") could otherwise be read as requiring a
# duplicate fixture.
#
# The remaining focused tests cover every refusal condition PLAN.md section
# 26.6 lists (stale meter, auth required, non-current or missing calibration,
# an incomplete cost model, a plan-tier mismatch, thin or estimated-only
# history, attribution below the floor) and the single invocation that lists
# every applicable one at once, none of which a shell case can seed without
# reaching into calibration and cost-model internals the CLI does not expose.

set -euo pipefail

WORKFLOW_ID="3"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
E2E_RUNNER="$REPO_ROOT/tests/e2e/run.sh"
CASE_SOURCE_DIR="$REPO_ROOT/tests/e2e/cases"

die() {
    echo "workflow $WORKFLOW_ID: $*" >&2
    exit 1
}

command -v jq >/dev/null 2>&1 || die "jq is required by the E2E runner"

WORK_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/aub-workflow-3-XXXXXX")"
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

for case_name in 026-can-run.sh; do
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
    die "the can-run workflow did not run exactly one composed case"
assert_case_passed 026-can-run

run_focused_test() {
    local label="$1"
    shift
    local log="$TEST_LOGS/$label.log"
    if ! cargo test "$@" -- --test-threads=1 --nocapture >"$log" 2>&1; then
        cat "$log" >&2
        die "focused proof $label failed; log: $log"
    fi
}

# PLAN.md section 51's worked example, reproduced losslessly through the
# entire join from raw windows and calibrations to rendered text: the
# lower-percentage window (account:5h, 38.0%) holds three times the work of
# the limiting one (model-x:weekly, 52.0%), and judging by percentage alone
# would have answered AMPLE instead of the correct MARGINAL.
run_focused_test worked-example-verbatim --lib \
    golden_worked_example_renders_the_designed_structure

# Every refusal condition PLAN.md section 26.6 enumerates: a stale meter,
# authentication required, a non-current or missing calibration, an
# incomplete cost model, a plan-tier mismatch, and thin or estimated-only
# task history.
run_focused_test every-refusal-condition --lib refusal_

# A single invocation lists every applicable refusal at once rather than one
# per run, which is the exact property that makes `can-run`'s refusal safe to
# trust: a caller who fixes the first reported cause does not get surprised
# by a second one the same invocation already knew about.
run_focused_test single-invocation-lists-every-refusal --lib \
    integration_a_single_invocation_lists_every_applicable_refusal

echo "workflow $WORKFLOW_ID passed; runner log: $RUN_DIR"
