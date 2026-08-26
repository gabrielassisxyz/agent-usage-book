#!/usr/bin/env bash
# End-to-end runner: drives the release binary against isolated state directories
# and composes per-bead case files into a per-run log directory.
#
# The case protocol is the load-bearing part. Each case is one file under the cases
# directory that declares its own preconditions, steps and assertions; the runner
# composes them and owns none of their content. Twenty-nine beads downstream each
# contribute one case file, so the shape here is what keeps their edit surfaces
# disjoint under a shared tree.
#
# Usage:
#   tests/e2e/run.sh [--state-dir DIR] [--cases-dir DIR] [--runs-dir DIR]
#                    [--keep N] [--self-test] [--help]
#
# The runner refuses to start when its state directory resolves to the configured
# operator state directory (AUB_STATE_DIR, or the default below), because it creates
# and destroys state directories in a loop and the operator's holds the one series
# that cannot be reconstructed.
set -uo pipefail

E2E_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$E2E_DIR/../.." && pwd)"

CASES_DIR="${CASES_DIR:-$E2E_DIR/cases}"
RUNS_DIR="${RUNS_DIR:-$E2E_DIR/runs}"
RUN_KEEP=10
SCHEMA_VERSION=1
# The operator's real state directory. The runner must never resolve to this.
OPERATOR_STATE_DIR="${AUB_STATE_DIR:-$HOME/.local/state/aub}"

# --- small helpers -----------------------------------------------------------

die() {
    echo "run.sh: $*" >&2
    exit 1
}

# A monotonic clock in nanoseconds, from /proc/uptime where available, else the
# wall clock. The runner records both; monotonic is what makes a duration immune to
# an NTP step or a suspend.
monotonic_ns() {
    if [ -r /proc/uptime ]; then
        read -r up _ </proc/uptime
        # uptime is seconds with two decimals; scale to nanoseconds.
        awk -v up="$up" 'BEGIN { printf "%d", up * 1000000000 }'
    else
        date +%s%N
    fi
}

# A digest of the state directory's content, so a step can prove what it changed.
state_digest() {
    local dir="$1"
    if [ -d "$dir" ]; then
        (cd "$dir" && find . -type f -print0 | sort -z | xargs -0 sha256sum 2>/dev/null | sha256sum | cut -d' ' -f1)
    else
        echo "absent"
    fi
}

# Resolves a path to its canonical form (symlinks and .. resolved) for the refusal
# comparison, so a path that merely spells the operator directory differently is
# still refused.
resolve_path() {
    local path="$1"
    if [ -e "$path" ]; then
        readlink -f "$path"
    else
        # readlink -f resolves even nonexistent paths on GNU coreutils.
        readlink -f "$path" 2>/dev/null || echo "$path"
    fi
}

# --- case protocol helpers ---------------------------------------------------

# The helpers below are the entire interface a case file sees. A case file is
# sourced, so it runs in this shell and may call these directly.

AUB_BIN="${AUB_BIN:-}"
STATE_DIR=""
CASE_LOG_DIR=""
CASE_STEPS_DIR=""
STEP_N=0
STEP_DIRS=()
CASE_FAILED=0
CASE_ASSERTIONS=()

# require_command CMD: a precondition. Fails the case when CMD is absent.
require_command() {
    if ! command -v "$1" >/dev/null 2>&1; then
        record_assertion "require_command $1" "present" "absent" "fail"
        CASE_FAILED=1
    fi
}

# step NAME COMMAND...: runs one command, captures its argument vector, stdout,
# stderr, exit status or signal, wall and monotonic duration, and the state digest
# before and after. Returns the step number.
step() {
    local name="$1"
    shift
    STEP_N=$((STEP_N + 1))
    local dir="$CASE_STEPS_DIR/$STEP_N-$name"
    mkdir -p "$dir"
    STEP_DIRS[$STEP_N]="$dir"

    # Exact argument vector, NUL-separated so spaces and newlines survive.
    printf '%s\0' "$@" >"$dir/argv.bin"

    local start_wall start_mono end_wall end_mono rc
    start_wall="$(date +%s%N)"
    start_mono="$(monotonic_ns)"
    state_digest "$STATE_DIR" >"$dir/state-before.sha256"

    set +e
    "$@" >"$dir/stdout.bin" 2>"$dir/stderr.bin"
    rc=$?

    end_wall="$(date +%s%N)"
    end_mono="$(monotonic_ns)"
    state_digest "$STATE_DIR" >"$dir/state-after.sha256"

    if [ "$rc" -gt 128 ]; then
        printf 'signal:%d\n' "$((rc - 128))" >"$dir/exit.txt"
    else
        printf 'exit:%d\n' "$rc" >"$dir/exit.txt"
    fi
    printf 'wall_ns:%d\nmono_ns:%d\n' "$((end_wall - start_wall))" "$((end_mono - start_mono))" >"$dir/duration.txt"

    # Decoded views alongside the lossless byte artifacts.
    iconv -f UTF-8 -t UTF-8 "$dir/stdout.bin" >"$dir/stdout.txt" 2>/dev/null || cp "$dir/stdout.bin" "$dir/stdout.txt"
    iconv -f UTF-8 -t UTF-8 "$dir/stderr.bin" >"$dir/stderr.txt" 2>/dev/null || cp "$dir/stderr.bin" "$dir/stderr.txt"
}

# step_dir N: the directory holding step N's artifacts.
step_dir() {
    echo "${STEP_DIRS[$1]}"
}

# step_exit N: the exit status (or signal) of step N, as "exit:N" or "signal:N".
step_exit() {
    cat "$(step_dir "$1")/exit.txt"
}

# record_assertion NAME EXPECTED OBSERVED VERDICT: appends one assertion result.
record_assertion() {
    CASE_ASSERTIONS+=("$1|$2|$3|$4")
}

# assert_exit EXPECTED STEP: the step's exit status equals EXPECTED.
assert_exit() {
    local expected="$1" step="$2"
    local observed
    observed="$(step_exit "$step")"
    if [ "$observed" = "exit:$expected" ]; then
        record_assertion "assert_exit $expected step $step" "exit:$expected" "$observed" "pass"
    else
        record_assertion "assert_exit $expected step $step" "exit:$expected" "$observed" "fail"
        CASE_FAILED=1
    fi
}

# assert_stdout_contains STEP TEXT: the step's stdout contains TEXT.
assert_stdout_contains() {
    local step="$1" text="$2"
    if grep -qF -- "$text" "$(step_dir "$step")/stdout.bin"; then
        record_assertion "assert_stdout_contains step $step" "contains:$text" "contains:$text" "pass"
    else
        record_assertion "assert_stdout_contains step $step" "contains:$text" "absent" "fail"
        CASE_FAILED=1
    fi
}

# assert_stderr_contains STEP TEXT: the step's stderr contains TEXT.
assert_stderr_contains() {
    local step="$1" text="$2"
    if grep -qF -- "$text" "$(step_dir "$step")/stderr.bin"; then
        record_assertion "assert_stderr_contains step $step" "contains:$text" "contains:$text" "pass"
    else
        record_assertion "assert_stderr_contains step $step" "contains:$text" "absent" "fail"
        CASE_FAILED=1
    fi
}

# assert_json_field STEP FIELD VALUE: the step's stdout is JSON and FIELD equals VALUE.
assert_json_field() {
    local step="$1" field="$2" value="$3"
    local observed
    observed="$(jq -r ".$field // empty" "$(step_dir "$step")/stdout.bin" 2>/dev/null)"
    if [ "$observed" = "$value" ]; then
        record_assertion "assert_json_field $field step $step" "$value" "$observed" "pass"
    else
        record_assertion "assert_json_field $field step $step" "$value" "$observed" "fail"
        CASE_FAILED=1
    fi
}

# --- case execution ----------------------------------------------------------

# run_case CASE_FILE: sources the case and runs its preconditions, steps and
# assertions in a fresh state directory, writing the case log.
run_case() {
    local case_file="$1"
    local case_id case_description

    # shellcheck disable=SC1090
    source "$case_file"

    case_id="${CASE_ID:-$(basename "$case_file" .sh)}"
    case_description="${CASE_DESCRIPTION:-}"

    local case_log_dir="$RUN_DIR/cases/$case_id"
    CASE_LOG_DIR="$case_log_dir"
    CASE_STEPS_DIR="$case_log_dir/steps"
    mkdir -p "$CASE_STEPS_DIR"

    # A fresh state directory per case: a case cannot observe another case's state.
    STATE_DIR="$(mktemp -d "$RUN_DIR/state-$case_id-XXXXXX")"
    chmod 700 "$STATE_DIR"

    STEP_N=0
    STEP_DIRS=()
    CASE_FAILED=0
    CASE_ASSERTIONS=()

    local start_ns end_ns
    start_ns="$(date +%s%N)"

    if declare -f case_preconditions >/dev/null 2>&1; then
        case_preconditions
    fi
    if declare -f case_steps >/dev/null 2>&1; then
        case_steps
    fi
    if declare -f case_assertions >/dev/null 2>&1; then
        case_assertions
    fi

    end_ns="$(date +%s%N)"

    local verdict="pass"
    [ "$CASE_FAILED" -ne 0 ] && verdict="fail"

    # Case metadata: identifier, description, verdict, duration, and the runner and
    # binary identity plus the sanitized environment and state digest.
    {
        echo "case_id: $case_id"
        echo "description: $case_description"
        echo "verdict: $verdict"
        echo "duration_ns: $((end_ns - start_ns))"
        echo "runner_revision: $RUNNER_REVISION"
        echo "binary_revision: $BINARY_REVISION"
        echo "binary_digest: $BINARY_DIGEST"
        echo "schema_version: $SCHEMA_VERSION"
        echo "state_dir: $STATE_DIR"
        echo "state_digest_after: $(state_digest "$STATE_DIR")"
        echo "allowlisted_env: AUB_LOG_LEVEL"
        echo "injected_clock: none"
        echo "synthetic_server_endpoint: none"
        echo "fixture_hashes: none"
    } >"$case_log_dir/case.log"

    # Assertion results, one per line, for the machine-readable summary.
    printf '%s\n' "${CASE_ASSERTIONS[@]}" >"$case_log_dir/assertions.txt"

    # A failing case must be diagnosable from the run directory alone: name the
    # failed assertions and the preserved run directory.
    if [ "$CASE_FAILED" -ne 0 ]; then
        {
            echo "case $case_id FAILED"
            for assertion in "${CASE_ASSERTIONS[@]}"; do
                IFS='|' read -r name expected observed verdict <<<"$assertion"
                [ "$verdict" = "fail" ] && echo "  $name: expected $expected, observed $observed"
            done
            echo "  run directory: $RUN_DIR"
        } >&2
    fi

    echo "$case_id|$verdict|$((end_ns - start_ns))"
}

# --- run log -----------------------------------------------------------------

write_timeline() {
    local timeline="$RUN_DIR/timeline.txt"
    {
        echo "run $RUN_ID"
        echo "  runner revision: $RUNNER_REVISION"
        echo "  binary revision: $BINARY_REVISION"
        echo "  binary digest: $BINARY_DIGEST"
        echo
        for entry in "${CASE_RESULTS[@]}"; do
            IFS='|' read -r id verdict duration <<<"$entry"
            echo "  case $id: ${verdict^^} ($((duration / 1000000))ms)"
            local case_log="$RUN_DIR/cases/$id/case.log"
            if [ -f "$case_log" ]; then
                while IFS= read -r line; do
                    case "$line" in
                        "assert_"*) echo "    $line" ;;
                    esac
                done <"$case_log"
            fi
        done
    } >"$timeline"
}

write_summary() {
    local summary="$RUN_DIR/summary.json"
    {
        echo '{'
        echo "  \"schema\": $SCHEMA_VERSION,"
        echo "  \"run_id\": \"$RUN_ID\","
        echo "  \"runner_revision\": \"$RUNNER_REVISION\","
        echo "  \"binary_revision\": \"$BINARY_REVISION\","
        echo "  \"binary_digest\": \"$BINARY_DIGEST\","
        echo '  "cases": ['
        local first=1
        for entry in "${CASE_RESULTS[@]}"; do
            IFS='|' read -r id verdict duration <<<"$entry"
            [ "$first" -eq 1 ] && first=0 || echo ','
            echo "    {\"id\": \"$id\", \"verdict\": \"$verdict\", \"duration_ns\": $duration}"
        done
        echo
        echo '  ]'
        echo '}'
    } >"$summary"
}

# --- pruning -----------------------------------------------------------------

prune_runs() {
    local keep="$1"
    local runs
    runs="$(find "$RUNS_DIR" -mindepth 1 -maxdepth 1 -type d -printf '%T@ %p\n' 2>/dev/null | sort -rn | tail -n +"$((keep + 1))" | cut -d' ' -f2-)"
    for old in $runs; do
        rm -rf "$old"
    done
}

# --- refusal -----------------------------------------------------------------

refuse_if_operator_state() {
    local resolved_operator resolved_state
    resolved_operator="$(resolve_path "$OPERATOR_STATE_DIR")"
    resolved_state="$(resolve_path "$STATE_ROOT")"
    if [ "$resolved_state" = "$resolved_operator" ]; then
        die "refusing to run: state directory $STATE_ROOT resolves to the operator state directory $OPERATOR_STATE_DIR"
    fi
}

# --- consistency -------------------------------------------------------------

# check_consistency: every command in the command surface must have a case file
# that exercises it, so a command cannot ship unexercised. The surface is one
# command per line in tests/e2e/command-surface.txt.
check_consistency() {
    local surface="$E2E_DIR/command-surface.txt"
    [ -f "$surface" ] || die "command surface file not found: $surface"

    local missing=0
    local command
    while IFS= read -r command; do
        case "$command" in
            ''|\#*) continue ;;
        esac
        if ! grep -qF -- "$command" "$CASES_DIR"/*.sh 2>/dev/null; then
            echo "consistency: no end-to-end case for command '$command'" >&2
            missing=1
        fi
    done <"$surface"

    if [ "$missing" -ne 0 ]; then
        echo "consistency: FAILED" >&2
        exit 1
    fi
    echo "consistency: every command has a case"
}

# --- self-test ---------------------------------------------------------------

self_test() {
    local tmp
    tmp="$(mktemp -d)"
    local fake_bin="$tmp/fake-aub"
    cat >"$fake_bin" <<'FAKE'
#!/usr/bin/env bash
case "${1:-}" in
    status) echo '{"status":"ok"}'; exit 0 ;;
    fail) echo "about to fail" >&2; exit 3 ;;
    hang) sleep 30; exit 0 ;;
    boom) kill -TERM $$ ;;
    *) echo "fake-aub"; exit 0 ;;
esac
FAKE
    chmod +x "$fake_bin"

    local cases="$tmp/cases"
    mkdir -p "$cases"
    cat >"$cases/001-status.sh" <<'CASE'
CASE_ID="001-status"
CASE_DESCRIPTION="status prints a JSON object and exits zero."
case_steps() {
    step "status" "$AUB_BIN" status
}
case_assertions() {
    assert_exit 0 1
    assert_json_field 1 status ok
}
CASE
    cat >"$cases/002-fail.sh" <<'CASE'
CASE_ID="002-fail"
CASE_DESCRIPTION="a failing child is reported with its exit class."
case_steps() {
    step "fail" "$AUB_BIN" fail
}
case_assertions() {
    assert_exit 0 1
}
CASE

    local runs="$tmp/runs"
    local out
    out="$(AUB_BIN="$fake_bin" CASES_DIR="$cases" RUNS_DIR="$runs" STATE_ROOT="$tmp/state" \
        bash "$0" --state-dir "$tmp/state" --cases-dir "$cases" --runs-dir "$runs" 2>&1)"
    local rc=$?

    # A failing case fails the run (exit 1), but the run still completes and
    # records every case, which is what the checks below prove.
    [ "$rc" -eq 1 ] || { echo "self-test: runner exited $rc, expected 1: $out" >&2; rm -rf "$tmp"; return 1; }

    # Complete artifacts: every case has a case.log, and every step has the full
    # artifact set.
    for case_id in 001-status 002-fail; do
        [ -f "$runs"/*/cases/"$case_id"/case.log ] || { echo "self-test: missing case.log for $case_id" >&2; rm -rf "$tmp"; return 1; }
    done
    local step_dir
    step_dir="$(find "$runs" -type d -name '1-status' | head -1)"
    for artifact in argv.bin stdout.bin stderr.bin stdout.txt stderr.txt exit.txt duration.txt state-before.sha256 state-after.sha256; do
        [ -f "$step_dir/$artifact" ] || { echo "self-test: missing $artifact" >&2; rm -rf "$tmp"; return 1; }
    done

    # The failing case is recorded as fail, not as a runner crash.
    grep -q '"verdict": "fail"' "$runs"/*/summary.json || { echo "self-test: failing case not recorded" >&2; rm -rf "$tmp"; return 1; }

    # Refusal: running against the operator state directory must refuse.
    if AUB_BIN="$fake_bin" CASES_DIR="$cases" RUNS_DIR="$runs" STATE_ROOT="$OPERATOR_STATE_DIR" \
        bash "$0" --state-dir "$OPERATOR_STATE_DIR" --cases-dir "$cases" --runs-dir "$runs" >/dev/null 2>&1; then
        echo "self-test: operator-state refusal did not fire" >&2
        rm -rf "$tmp"
        return 1
    fi

    rm -rf "$tmp"
    echo "self-test: ok"
}

# --- main --------------------------------------------------------------------

usage() {
    cat <<'USAGE'
Usage: tests/e2e/run.sh [--state-dir DIR] [--cases-dir DIR] [--runs-dir DIR]
                       [--keep N] [--check-consistency] [--self-test] [--help]

Runs every case file under the cases directory against the release binary, each in
a fresh state directory, and writes a per-run log directory under the runs
directory. The runner refuses to start when its state directory resolves to the
configured operator state directory. --check-consistency verifies every command in
the command surface has a case file and exits without running the suite.
USAGE
}

main() {
    STATE_ROOT=""
    AUB_BIN="${AUB_BIN:-$REPO_ROOT/target/release/aub}"

    while [ "$#" -gt 0 ]; do
        case "$1" in
            --state-dir) STATE_ROOT="$2"; shift 2 ;;
            --cases-dir) CASES_DIR="$2"; shift 2 ;;
            --runs-dir) RUNS_DIR="$2"; shift 2 ;;
            --keep) RUN_KEEP="$2"; shift 2 ;;
            --check-consistency) check_consistency; exit $? ;;
            --self-test) self_test; exit $? ;;
            --help|-h) usage; exit 0 ;;
            *) die "unknown argument: $1" ;;
        esac
    done

    [ -n "$STATE_ROOT" ] || STATE_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/aub-e2e-state-XXXXXX")"
    mkdir -p "$STATE_ROOT"

    refuse_if_operator_state

    [ -x "$AUB_BIN" ] || die "release binary not found at $AUB_BIN; build it first (cargo build --release)"

    RUNNER_REVISION="$(git -C "$REPO_ROOT" rev-parse HEAD 2>/dev/null || echo unknown)"
    BINARY_REVISION="$("$AUB_BIN" 2>/dev/null | awk '{print $NF}' | tr -d '()' || echo unknown)"
    BINARY_DIGEST="$(sha256sum "$AUB_BIN" | cut -d' ' -f1)"

    RUN_ID="$(date +%Y%m%d-%H%M%S)-$$"
    RUN_DIR="$RUNS_DIR/$RUN_ID"
    mkdir -p "$RUN_DIR"

    CASE_RESULTS=()
    local case_file
    while IFS= read -r -d '' case_file; do
        CASE_RESULTS+=("$(run_case "$case_file")")
    done < <(find "$CASES_DIR" -maxdepth 1 -type f -name '*.sh' -print0 | sort -z)

    write_timeline
    write_summary
    prune_runs "$RUN_KEEP"

    # Report the run: a failing case is a failing run.
    local failed=0
    for entry in "${CASE_RESULTS[@]}"; do
        IFS='|' read -r _ verdict _ <<<"$entry"
        [ "$verdict" = "fail" ] && failed=1
    done

    echo "run $RUN_ID: ${#CASE_RESULTS[@]} cases, log at $RUN_DIR"
    [ "$failed" -eq 0 ] || { echo "run $RUN_ID FAILED" >&2; exit 1; }
}

main "$@"
