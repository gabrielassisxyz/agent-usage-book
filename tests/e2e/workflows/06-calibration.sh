#!/usr/bin/env bash
# Workflow 6 composes the release-binary calibration lifecycle cases: a
# controlled experiment's begin/status/end across separate processes, passive
# candidate generation that never activates, show/history/compare/activate
# with their refusals, and the single-source proof that every consumer
# (calibrate show, spend --window-equivalent, can-run --cached) moves to an
# append-only successor with no source or configuration edit. Focused tests
# add the two steps no case file drives through the CLI yet: a real fit
# becoming an immutable, never-auto-activated candidate, and a comparison
# between two distinct calibrations reporting a non-zero difference (the
# composed cases only ever compare a record against itself).

set -euo pipefail

WORKFLOW_ID="6"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
E2E_RUNNER="$REPO_ROOT/tests/e2e/run.sh"
CASE_SOURCE_DIR="$REPO_ROOT/tests/e2e/cases"

die() {
    echo "workflow $WORKFLOW_ID: $*" >&2
    exit 1
}

command -v jq >/dev/null 2>&1 || die "jq is required by the E2E runner"

WORK_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/aub-workflow-6-XXXXXX")"
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

for case_name in \
    023-calibrate-controlled-experiment.sh \
    026-calibrate-passive.sh \
    027-calibrate-show-history-compare-activate.sh \
    029-can-run-calibration-single-source.sh; do
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

[[ "$(jq -r '.cases | length' "$SUMMARY")" == 4 ]] || \
    die "the calibration workflow did not run exactly four composed cases"
for case_id in \
    023-calibrate-controlled-experiment \
    026-calibrate-passive \
    027-calibrate-show-history-compare-activate \
    029-can-run-calibration-single-source; do
    assert_case_passed "$case_id"
done

run_focused_test() {
    local label="$1"
    shift
    local log="$TEST_LOGS/$label.log"
    if ! cargo test "$@" -- --test-threads=1 --nocapture >"$log" 2>&1; then
        cat "$log" >&2
        die "focused proof $label failed; log: $log"
    fi
}

# A fit becomes a candidate: the release binary's own `calibrate fit` writes
# an immutable window_calibration_candidate row within the seeded coefficient's
# uncertainty interval, rejects any later update or delete, and never writes a
# calibration_lifecycle row, because a fit is evidence, not an activation.
run_focused_test fit-becomes-an-immutable-unactivated-candidate --test calibrate_fit_command \
    test_calibrate_fit_release_binary_quantized_success

# Comparison reports a difference: the composed cases only ever compare a
# record against itself (0.0%, "candidate is active"), so this is the one
# proof that two distinct calibrations produce a real, rendered percentage
# difference rather than always reporting zero.
run_focused_test compare-reports-a-real-difference --lib \
    calibrate_compare_renders_the_design_example

# Activation is explicit: the CLI has no path that activates a calibration as
# a side effect of a fit or a passive candidate, so this proof exercises the
# one command that does, naming the actor and the exact policy version under
# which the record became current.
run_focused_test activation-is-explicit --lib \
    calibrate_activate_records_the_explicit_activation

echo "workflow $WORKFLOW_ID passed; runner log: $RUN_DIR"
