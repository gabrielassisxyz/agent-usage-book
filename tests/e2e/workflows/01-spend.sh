#!/usr/bin/env bash
# Workflow 1 composes the release-binary `aub spend` cases that exercise the
# opt-in dimensions (credits, valuation, window-equivalent), the day/session/
# project/repository/account grouping surface, and the account explain
# provenance. Focused tests add the two seams no case file drives through the
# CLI yet: `--group-by task` reconciling to the canonical total, and a real
# unreadable transcript file turning into a qualified known subtotal.
#
# Workflow 3's plan.md worked example is reused there rather than duplicated
# here: `026-can-run.sh` and its focused tests already own the calibrated
# headroom comparison this workflow's spend grouping only feeds, not asserts.

set -euo pipefail

WORKFLOW_ID="1"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
E2E_RUNNER="$REPO_ROOT/tests/e2e/run.sh"
CASE_SOURCE_DIR="$REPO_ROOT/tests/e2e/cases"

die() {
    echo "workflow $WORKFLOW_ID: $*" >&2
    exit 1
}

command -v jq >/dev/null 2>&1 || die "jq is required by the E2E runner"

WORK_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/aub-workflow-1-XXXXXX")"
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
    007-spend.sh \
    016-spend-valuation.sh \
    017-spend-credits.sh \
    020-spend-account.sh; do
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
    die "the spend workflow did not run exactly four composed cases"
for case_id in \
    007-spend \
    016-spend-valuation \
    017-spend-credits \
    020-spend-account; do
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

# `--group-by task` is the sixth grouping dimension this workflow owns (day,
# session, project, repository and account come from the cases above); no
# case file drives it yet, so the source-level reconciliation proof stands in.
run_focused_test group-by-task-reconciles --lib \
    group_by_task_reconciles_to_canonical_totals_and_labels_by_task_and_overhead

# A real unreadable transcript file, named by the real ingest engine rather
# than simulated, is the failed-transcript half of this workflow's own
# acceptance criterion.
run_focused_test unreadable-transcript-file-is-named --test zero_is_data_and_no_silent_fallback \
    transcript_file_that_cannot_be_read_is_named_not_silently_dropped

# The qualification that failure produces: human output says "refresh
# incomplete" rather than a bare total, and JSON carries the same fact as an
# explicit `refresh_failure` field, never an unqualified total either way.
run_focused_test failed-refresh-qualifies-known-subtotal --lib \
    a_failed_refresh_qualifies_the_known_canonical_subtotal

echo "workflow $WORKFLOW_ID passed; runner log: $RUN_DIR"
