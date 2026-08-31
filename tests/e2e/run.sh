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
STEP_EXIT_TOKENS=()
CASE_FAILED=0
CASE_PHASE="pass"
CASE_ASSERTIONS=()
DECLARED_GOLDEN_STEPS=()

# json_string VALUE: a JSON-quoted, escaped string, for values that may contain
# quotes or backslashes (assertion text drawn from a step's own stdout).
json_string() {
    local s="$1"
    s="${s//\\/\\\\}"
    s="${s//\"/\\\"}"
    printf '"%s"' "$s"
}

# require_command CMD: a precondition. Fails the case when CMD is absent.
require_command() {
    if ! command -v "$1" >/dev/null 2>&1; then
        record_assertion "require_command $1" "present" "absent" "fail"
        CASE_FAILED=1
    fi
}

# step [--timeout SECS] NAME COMMAND...: runs one command, captures its argument
# vector, stdout, stderr, exit status, signal or timeout, wall and monotonic
# duration, and the state digest before and after. Returns the step number.
# --timeout bounds the child with SIGTERM then SIGKILL after a 2s grace period;
# a step that hits it is recorded as "timeout:SECSs" rather than "exit:124", so a
# real exit code of 124 from the child itself is never confused with the runner's
# own timeout.
step() {
    local timeout_secs=""
    if [ "${1:-}" = "--timeout" ]; then
        timeout_secs="$2"
        shift 2
    fi
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
    if [ -n "$timeout_secs" ]; then
        timeout --signal=TERM --kill-after=2 "$timeout_secs" "$@" >"$dir/stdout.bin" 2>"$dir/stderr.bin"
    else
        "$@" >"$dir/stdout.bin" 2>"$dir/stderr.bin"
    fi
    rc=$?

    end_wall="$(date +%s%N)"
    end_mono="$(monotonic_ns)"
    state_digest "$STATE_DIR" >"$dir/state-after.sha256"

    if [ -n "$timeout_secs" ] && [ "$rc" -eq 124 ]; then
        printf 'timeout:%ss\n' "$timeout_secs" >"$dir/exit.txt"
    elif [ "$rc" -gt 128 ]; then
        printf 'signal:%d\n' "$((rc - 128))" >"$dir/exit.txt"
    else
        printf 'exit:%d\n' "$rc" >"$dir/exit.txt"
    fi
    STEP_EXIT_TOKENS[$STEP_N]="$(cat "$dir/exit.txt")"
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

# record_assertion NAME EXPECTED OBSERVED VERDICT [KIND]: appends one assertion
# result. KIND defaults to "assertion"; the dedicated timeout and signal asserters
# pass "timeout" or "signal" so a failing case's phase reflects what the failing
# assertion was actually checking, not just that something failed.
record_assertion() {
    local kind="${5:-assertion}"
    CASE_ASSERTIONS+=("$1|$2|$3|$4|$kind")
    if [ "$4" = "fail" ] && [ "$CASE_PHASE" = "pass" ]; then
        case "$kind" in
            timeout) CASE_PHASE="timeout" ;;
            signal) CASE_PHASE="signal" ;;
            *) CASE_PHASE="assertion_failure" ;;
        esac
    fi
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

# assert_timeout STEP: the step's own bounded timeout fired.
assert_timeout() {
    local step="$1"
    local observed
    observed="$(step_exit "$step")"
    case "$observed" in
        timeout:*)
            record_assertion "assert_timeout step $step" "timeout" "$observed" "pass" "timeout"
            ;;
        *)
            record_assertion "assert_timeout step $step" "timeout" "$observed" "fail" "timeout"
            CASE_FAILED=1
            ;;
    esac
}

# assert_signal EXPECTED STEP: the step was terminated by signal EXPECTED.
assert_signal() {
    local expected="$1" step="$2"
    local observed
    observed="$(step_exit "$step")"
    if [ "$observed" = "signal:$expected" ]; then
        record_assertion "assert_signal $expected step $step" "signal:$expected" "$observed" "pass" "signal"
    else
        record_assertion "assert_signal $expected step $step" "signal:$expected" "$observed" "fail" "signal"
        CASE_FAILED=1
    fi
}

# assert_golden STEP GOLDEN_FILE: the step's stdout matches a checked-in golden
# file byte for byte. Calling this IS the case's explicit declaration that its
# criterion is rendering; a mismatch writes a unified diff into the case log so
# the change is visible without re-running. This is the only rendered-prose
# comparison a case may declare.
assert_golden() {
    local step="$1" golden_file="$2"
    DECLARED_GOLDEN_STEPS+=("$step")
    local observed_file
    observed_file="$(step_dir "$step")/stdout.txt"
    if [ ! -f "$golden_file" ]; then
        record_assertion "assert_golden step $step" "golden:$golden_file" "missing" "fail"
        CASE_FAILED=1
        return
    fi
    if diff -u "$golden_file" "$observed_file" >"$CASE_LOG_DIR/golden-$step.diff" 2>&1; then
        rm -f "$CASE_LOG_DIR/golden-$step.diff"
        record_assertion "assert_golden step $step" "matches:$golden_file" "matches:$golden_file" "pass"
    else
        record_assertion "assert_golden step $step" "matches:$golden_file" "differs, see golden-$step.diff" "fail"
        CASE_FAILED=1
    fi
}

# assert_stdout_equals STEP TEXT: a literal, full-text comparison. Refused unless
# assert_golden has already declared the same step rendering-worthy: an
# undeclared rendered-prose assertion is exactly the case this guards against,
# because prose drifts for reasons that have nothing to do with the behaviour
# under test and a bare literal-equals assertion gives no diff to triage it by.
assert_stdout_equals() {
    local step="$1" text="$2"
    local declared=0 s
    for s in "${DECLARED_GOLDEN_STEPS[@]:-}"; do
        [ "$s" = "$step" ] && declared=1
    done
    if [ "$declared" -ne 1 ]; then
        record_assertion "assert_stdout_equals step $step" "declared-golden" "undeclared" "fail"
        CASE_FAILED=1
        return
    fi
    local observed
    observed="$(cat "$(step_dir "$step")/stdout.txt")"
    if [ "$observed" = "$text" ]; then
        record_assertion "assert_stdout_equals step $step" "$text" "$observed" "pass"
    else
        record_assertion "assert_stdout_equals step $step" "$text" "$observed" "fail"
        CASE_FAILED=1
    fi
}

# --- case execution ----------------------------------------------------------

# run_case CASE_FILE: sources the case and runs its preconditions, steps and
# assertions in a fresh state directory, writing the case log.
run_case() {
    local case_file="$1"
    local case_id case_description

    # A case file is sourced into this shell, so a function or variable it does
    # not redefine would otherwise leak from whichever case ran before it. Clear
    # the whole case-declared surface first, which is as much a part of "a case
    # cannot observe another case's state" as the fresh state directory below.
    unset -f case_preconditions case_steps case_assertions 2>/dev/null
    unset CASE_ID CASE_DESCRIPTION CASE_SYNTHETIC_SERVER_ENDPOINT CASE_INJECTED_CLOCK

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
    STEP_EXIT_TOKENS=()
    CASE_FAILED=0
    CASE_PHASE="pass"
    CASE_ASSERTIONS=()
    DECLARED_GOLDEN_STEPS=()

    local start_ns end_ns
    start_ns="$(date +%s%N)"

    if declare -f case_preconditions >/dev/null 2>&1; then
        case_preconditions
    fi

    # The fixture manifest is the seed a case starts from: every file its own
    # preconditions wrote before any step ran. State a step creates afterward is
    # the behaviour under test, not a fixture, so this snapshot is taken here and
    # nowhere else.
    local fixture_hashes="none" fixture_list="" f
    if [ -d "$STATE_DIR" ]; then
        while IFS= read -r -d '' f; do
            fixture_list+="${f#"$STATE_DIR"/}=$(sha256sum "$f" | cut -d' ' -f1);"
        done < <(find "$STATE_DIR" -type f -print0 2>/dev/null | sort -z)
    fi
    [ -n "$fixture_list" ] && fixture_hashes="${fixture_list%;}"

    if declare -f case_steps >/dev/null 2>&1; then
        case_steps
    fi
    if declare -f case_assertions >/dev/null 2>&1; then
        case_assertions
    fi

    end_ns="$(date +%s%N)"

    local verdict="pass"
    [ "$CASE_FAILED" -ne 0 ] && verdict="fail"

    # The env-var overrides a case actually passed to its own steps, recovered
    # from the argument vectors this runner already captured. This is real,
    # case-specific data rather than a fixed literal: a case that sets no
    # environment reports "none", and one that does names exactly what it set.
    local env_assignments allowlisted_env="none" resolved_configuration="none"
    env_assignments="$(
        for f in "$CASE_STEPS_DIR"/*/argv.bin; do
            [ -f "$f" ] || continue
            local tok
            while IFS= read -r -d '' tok; do
                case "$tok" in
                    [A-Z_]*=*) printf '%s\n' "$tok" ;;
                esac
            done <"$f"
        done | sort -u
    )"
    [ -n "$env_assignments" ] && allowlisted_env="$(printf '%s' "$env_assignments" | tr '\n' ';' | sed 's/;$//')"
    local config_assignment config_path
    config_assignment="$(printf '%s\n' "$env_assignments" | grep '^AUB_CONFIG_FILE=' | head -1)"
    if [ -n "$config_assignment" ]; then
        config_path="${config_assignment#AUB_CONFIG_FILE=}"
        if [ -f "$config_path" ]; then
            resolved_configuration="$config_path=$(sha256sum "$config_path" | cut -d' ' -f1)"
        else
            resolved_configuration="$config_path=absent"
        fi
    fi

    # Case metadata: identifier, description, verdict, phase, duration, and the
    # runner and binary identity plus the sanitized environment and state digest.
    {
        echo "case_id: $case_id"
        echo "description: $case_description"
        echo "verdict: $verdict"
        echo "phase: $CASE_PHASE"
        echo "duration_ns: $((end_ns - start_ns))"
        echo "runner_revision: $RUNNER_REVISION"
        echo "binary_revision: $BINARY_REVISION"
        echo "binary_digest: $BINARY_DIGEST"
        echo "schema_version: $SCHEMA_VERSION"
        echo "state_dir: $STATE_DIR"
        echo "state_digest_after: $(state_digest "$STATE_DIR")"
        echo "resolved_configuration: $resolved_configuration"
        echo "allowlisted_env: $allowlisted_env"
        echo "injected_clock: ${CASE_INJECTED_CLOCK:-none}"
        echo "synthetic_server_endpoint: ${CASE_SYNTHETIC_SERVER_ENDPOINT:-none}"
        echo "fixture_hashes: $fixture_hashes"
    } >"$case_log_dir/case.log"

    # Assertion results, one per line, for the machine-readable summary.
    printf '%s\n' "${CASE_ASSERTIONS[@]}" >"$case_log_dir/assertions.txt"

    # A failing, timed-out or signal-terminated case must be diagnosable from the
    # run directory alone: its phase, the exact sequence its steps terminated in,
    # the last structured diagnostic event any step emitted, every failed
    # assertion, and the run directory itself.
    if [ "$CASE_FAILED" -ne 0 ]; then
        local termination_sequence="" n last_event
        for ((n = 1; n <= STEP_N; n++)); do
            termination_sequence+="${n}:${STEP_EXIT_TOKENS[$n]:-unknown} "
        done
        last_event="$(cat "$CASE_STEPS_DIR"/*/stderr.bin 2>/dev/null | grep -o '{"ts":.*}' | tail -1)"
        [ -n "$last_event" ] || last_event="none"
        {
            echo "case $case_id FAILED"
            echo "phase: $CASE_PHASE"
            echo "termination_sequence: $termination_sequence"
            echo "last_structured_event: $last_event"
            echo "failed_assertions:"
            for assertion in "${CASE_ASSERTIONS[@]}"; do
                IFS='|' read -r name expected observed assertion_verdict kind <<<"$assertion"
                [ "$assertion_verdict" = "fail" ] && echo "  $name ($kind): expected $expected, observed $observed"
            done
            echo "run_directory: $RUN_DIR"
        } | tee "$case_log_dir/diagnosis.txt" >&2
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
            local phase
            phase="$(awk -F': ' '$1=="phase"{print $2}' "$RUN_DIR/cases/$id/case.log" 2>/dev/null)"
            [ -n "$phase" ] || phase="pass"
            echo "  case $id: ${verdict^^} (${duration}ns, phase=$phase)"
            # Every step, named with its outcome and duration, so a failure is
            # diagnosable from the timeline alone without re-running. The step
            # directory name carries the monotonic sequence number and the step
            # name; exit.txt holds the exit, signal or timeout token.
            local step_dir step_name exit_token wall mono
            for step_dir in "$RUN_DIR/cases/$id"/steps/*/; do
                [ -d "$step_dir" ] || continue
                step_name="$(basename "$step_dir")"
                exit_token="$(cat "$step_dir/exit.txt" 2>/dev/null || echo unknown)"
                wall="$(awk -F: '$1=="wall_ns"{print $2}' "$step_dir/duration.txt" 2>/dev/null)"
                mono="$(awk -F: '$1=="mono_ns"{print $2}' "$step_dir/duration.txt" 2>/dev/null)"
                echo "    step $step_name: $exit_token (wall ${wall:-unknown}ns, mono ${mono:-unknown}ns)"
            done
            local assertions_file="$RUN_DIR/cases/$id/assertions.txt"
            if [ -f "$assertions_file" ]; then
                while IFS='|' read -r aname aexpected aobserved averdict akind; do
                    [ -n "$aname" ] || continue
                    echo "    $aname [$averdict]: expected $aexpected observed $aobserved"
                done <"$assertions_file"
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
            local phase steps_count assertions_file
            phase="$(awk -F': ' '$1=="phase"{print $2}' "$RUN_DIR/cases/$id/case.log" 2>/dev/null)"
            [ -n "$phase" ] || phase="pass"
            steps_count="$(find "$RUN_DIR/cases/$id/steps" -mindepth 1 -maxdepth 1 -type d 2>/dev/null | wc -l)"
            printf '    {"id": %s, "verdict": %s, "duration_ns": %s, "phase": %s, "steps": %s, "assertions": [' \
                "$(json_string "$id")" "$(json_string "$verdict")" "$duration" "$(json_string "$phase")" "$steps_count"
            local afirst=1 aname aexpected aobserved averdict akind
            assertions_file="$RUN_DIR/cases/$id/assertions.txt"
            if [ -f "$assertions_file" ]; then
                while IFS='|' read -r aname aexpected aobserved averdict akind; do
                    [ -n "$aname" ] || continue
                    [ "$afirst" -eq 1 ] && afirst=0 || printf ','
                    printf '{"name": %s, "expected": %s, "observed": %s, "verdict": %s, "kind": %s}' \
                        "$(json_string "$aname")" "$(json_string "$aexpected")" "$(json_string "$aobserved")" \
                        "$(json_string "$averdict")" "$(json_string "$akind")"
                done <"$assertions_file"
            fi
            printf ']}'
        done
        echo
        echo '  ]'
        echo '}'
    } >"$summary"
}

# write_manifest: a content manifest of every artifact this run produced, so a
# consumer of summary.json or timeline.txt can verify the bytes they describe
# have not been altered since the run wrote them.
write_manifest() {
    local manifest="$RUN_DIR/manifest.json"
    {
        echo '{'
        echo '  "files": ['
        local first=1 f rel hash
        while IFS= read -r -d '' f; do
            rel="${f#"$RUN_DIR"/}"
            hash="$(sha256sum "$f" | cut -d' ' -f1)"
            [ "$first" -eq 1 ] && first=0 || echo ','
            printf '    {"path": %s, "sha256": "%s"}' "$(json_string "$rel")" "$hash"
        done < <(find "$RUN_DIR" -type f ! -name 'manifest.json' -print0 2>/dev/null | sort -z)
        echo
        echo '  ]'
        echo '}'
    } >"$manifest"
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
    local surface="${SURFACE_FILE:-$E2E_DIR/command-surface.txt}"
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
        return 1
    fi
    echo "consistency: every command has a case"
}

# --- self-test ---------------------------------------------------------------

self_test_basic() {
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
    cat >"$cases/003-clock.sh" <<'CASE'
CASE_ID="003-clock"
CASE_DESCRIPTION="a case that injects a clock records exactly what it injected."
CASE_INJECTED_CLOCK="fake-clock@2026-01-01T00:00:00Z"
case_steps() {
    step "status" "$AUB_BIN" status
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

    # A failing case's timeline names its step and its outcome, so the failure
    # is diagnosable from the run directory alone.
    grep -q "step 1-fail: exit:3" "$runs"/*/timeline.txt || { echo "self-test: failing case timeline missing its step outcome" >&2; rm -rf "$tmp"; return 1; }

    # A case-declared injected clock is recorded as the case declared it, not as
    # a fixed literal, and a case that declares none records none.
    grep -q "injected_clock: fake-clock@2026-01-01T00:00:00Z" "$runs"/*/cases/003-clock/case.log || { echo "self-test: declared injected clock not recorded" >&2; rm -rf "$tmp"; return 1; }
    grep -q "injected_clock: none" "$runs"/*/cases/001-status/case.log || { echo "self-test: absent injected clock not recorded as none" >&2; rm -rf "$tmp"; return 1; }

    # Refusal: running against the operator state directory must refuse.
    if AUB_BIN="$fake_bin" CASES_DIR="$cases" RUNS_DIR="$runs" STATE_ROOT="$OPERATOR_STATE_DIR" \
        bash "$0" --state-dir "$OPERATOR_STATE_DIR" --cases-dir "$cases" --runs-dir "$runs" >/dev/null 2>&1; then
        echo "self-test: operator-state refusal did not fire" >&2
        rm -rf "$tmp"
        return 1
    fi

    rm -rf "$tmp"
    echo "self-test: basic ok"
}

# self_test_pruning: prune_runs keeps exactly the newest KEEP run directories.
self_test_pruning() {
    local runs saved_runs_dir
    runs="$(mktemp -d)"
    mkdir -p "$runs/run-a" "$runs/run-b" "$runs/run-c"
    touch -d "2020-01-01" "$runs/run-a"
    touch -d "2020-01-02" "$runs/run-b"
    touch -d "2020-01-03" "$runs/run-c"

    saved_runs_dir="$RUNS_DIR"
    RUNS_DIR="$runs"
    prune_runs 2
    RUNS_DIR="$saved_runs_dir"

    local remaining
    remaining="$(find "$runs" -mindepth 1 -maxdepth 1 -type d | wc -l)"
    if [ "$remaining" -ne 2 ]; then
        echo "self-test: pruning left $remaining run dirs, expected 2" >&2
        rm -rf "$runs"
        return 1
    fi
    if [ -d "$runs/run-a" ]; then
        echo "self-test: pruning kept the oldest run instead of discarding it" >&2
        rm -rf "$runs"
        return 1
    fi

    rm -rf "$runs"
    echo "self-test: pruning ok"
}

# self_test_golden: an undeclared literal comparison is rejected, a declared
# golden that matches is accepted, and a declared golden that no longer matches
# fails with a diff on disk.
self_test_golden() {
    local tmp fake_bin golden_file
    tmp="$(mktemp -d)"
    fake_bin="$tmp/echo-aub"
    cat >"$fake_bin" <<'FAKE'
#!/usr/bin/env bash
echo "hello golden"
FAKE
    chmod +x "$fake_bin"
    golden_file="$tmp/golden.txt"
    printf 'hello golden\n' >"$golden_file"

    RUN_DIR="$tmp/run"
    CASE_LOG_DIR="$RUN_DIR/cases/g"
    CASE_STEPS_DIR="$CASE_LOG_DIR/steps"
    mkdir -p "$CASE_STEPS_DIR"
    STATE_DIR="$tmp/state"
    mkdir -p "$STATE_DIR"
    STEP_N=0
    STEP_DIRS=()
    STEP_EXIT_TOKENS=()
    CASE_FAILED=0
    CASE_PHASE="pass"
    CASE_ASSERTIONS=()
    DECLARED_GOLDEN_STEPS=()

    step "print" "$fake_bin"

    assert_stdout_equals 1 "hello golden"
    if [ "$CASE_FAILED" -eq 0 ]; then
        echo "self-test: an undeclared rendered-prose assertion was not rejected" >&2
        rm -rf "$tmp"
        return 1
    fi
    printf '%s\n' "${CASE_ASSERTIONS[@]}" | grep -q "undeclared" || {
        echo "self-test: undeclared rejection carried no reason" >&2
        rm -rf "$tmp"
        return 1
    }

    CASE_FAILED=0
    CASE_ASSERTIONS=()
    assert_golden 1 "$golden_file"
    if [ "$CASE_FAILED" -ne 0 ]; then
        echo "self-test: a matching declared golden was rejected" >&2
        rm -rf "$tmp"
        return 1
    fi

    printf 'something else\n' >"$golden_file"
    CASE_FAILED=0
    CASE_ASSERTIONS=()
    assert_golden 1 "$golden_file"
    if [ "$CASE_FAILED" -eq 0 ]; then
        echo "self-test: a mismatching declared golden was accepted" >&2
        rm -rf "$tmp"
        return 1
    fi
    [ -s "$CASE_LOG_DIR/golden-1.diff" ] || {
        echo "self-test: a mismatching golden produced no diff" >&2
        rm -rf "$tmp"
        return 1
    }

    rm -rf "$tmp"
    echo "self-test: golden declaration ok"
}

# self_test_consistency: a command covered by a case passes; the same surface
# with an uncovered command added fails, naming it.
self_test_consistency() {
    local tmp cases surface
    tmp="$(mktemp -d)"
    cases="$tmp/cases"
    mkdir -p "$cases"
    cat >"$cases/001-x.sh" <<'CASE'
CASE_ID="001-x"
# exercises: status
case_steps() { :; }
case_assertions() { :; }
CASE
    surface="$tmp/surface.txt"
    printf 'status\n' >"$surface"

    if ! (CASES_DIR="$cases" SURFACE_FILE="$surface" check_consistency >/dev/null 2>&1); then
        echo "self-test: consistency check failed a fully covered surface" >&2
        rm -rf "$tmp"
        return 1
    fi

    printf 'status\nspend\n' >"$surface"
    if (CASES_DIR="$cases" SURFACE_FILE="$surface" check_consistency >/dev/null 2>&1); then
        echo "self-test: consistency check missed an uncovered command" >&2
        rm -rf "$tmp"
        return 1
    fi

    rm -rf "$tmp"
    echo "self-test: consistency check ok"
}

# self_test_summary_parseback: summary.json, timeline.txt and manifest.json all
# describe the same run and agree with each other and with the files on disk.
self_test_summary_parseback() {
    command -v jq >/dev/null 2>&1 || { echo "self-test: jq required for parse-back check" >&2; return 1; }

    local tmp
    tmp="$(mktemp -d)"
    local fake_bin="$tmp/fake-aub"
    cat >"$fake_bin" <<'FAKE'
#!/usr/bin/env bash
case "${1:-}" in
    status) echo '{"status":"ok"}'; exit 0 ;;
    *) echo "fake-aub"; exit 0 ;;
esac
FAKE
    chmod +x "$fake_bin"
    local cases="$tmp/cases"
    mkdir -p "$cases"
    cat >"$cases/001-status.sh" <<'CASE'
CASE_ID="001-status"
case_steps() { step "status" "$AUB_BIN" status; }
case_assertions() { assert_exit 0 1; assert_json_field 1 status ok; }
CASE

    local runs="$tmp/runs"
    AUB_BIN="$fake_bin" CASES_DIR="$cases" RUNS_DIR="$runs" STATE_ROOT="$tmp/state" \
        bash "$0" --state-dir "$tmp/state" --cases-dir "$cases" --runs-dir "$runs" >/dev/null 2>&1

    local run_dir
    run_dir="$(find "$runs" -mindepth 1 -maxdepth 1 -type d | head -1)"
    [ -n "$run_dir" ] || { echo "self-test: no run directory produced" >&2; rm -rf "$tmp"; return 1; }

    local summary_verdict summary_steps summary_duration
    summary_verdict="$(jq -r '.cases[0].verdict' "$run_dir/summary.json")"
    summary_steps="$(jq -r '.cases[0].steps' "$run_dir/summary.json")"
    summary_duration="$(jq -r '.cases[0].duration_ns' "$run_dir/summary.json")"

    [ "$summary_verdict" = "pass" ] || { echo "self-test: summary verdict mismatch: $summary_verdict" >&2; rm -rf "$tmp"; return 1; }
    [ "$summary_steps" = "1" ] || { echo "self-test: summary step count mismatch: $summary_steps" >&2; rm -rf "$tmp"; return 1; }

    grep -q "case 001-status: PASS" "$run_dir/timeline.txt" || {
        echo "self-test: timeline verdict does not match summary" >&2
        rm -rf "$tmp"
        return 1
    }
    grep -q "step 1-status: exit:0" "$run_dir/timeline.txt" || {
        echo "self-test: timeline does not name the step and its outcome" >&2
        rm -rf "$tmp"
        return 1
    }
    grep -q "${summary_duration}ns" "$run_dir/timeline.txt" || {
        echo "self-test: timeline duration does not match summary" >&2
        rm -rf "$tmp"
        return 1
    }

    local step_stdout manifest_hash actual_hash
    step_stdout="$(find "$run_dir/cases/001-status/steps" -name stdout.bin | head -1)"
    manifest_hash="$(jq -r --arg p "${step_stdout#"$run_dir"/}" '.files[] | select(.path == $p) | .sha256' "$run_dir/manifest.json")"
    actual_hash="$(sha256sum "$step_stdout" | cut -d' ' -f1)"
    [ "$manifest_hash" = "$actual_hash" ] || {
        echo "self-test: manifest stream hash does not match the artifact on disk" >&2
        rm -rf "$tmp"
        return 1
    }

    rm -rf "$tmp"
    echo "self-test: summary parse-back ok"
}

# self_test_timeout_and_signal: a step can time out or be signalled and still be
# asserted on successfully, preserving its partial artifacts; and when the
# outcome is not the one asserted, the case reports the right phase.
self_test_timeout_and_signal() {
    local tmp
    tmp="$(mktemp -d)"
    local fake_bin="$tmp/fake-aub"
    cat >"$fake_bin" <<'FAKE'
#!/usr/bin/env bash
case "${1:-}" in
    hang) sleep 30; exit 0 ;;
    boom) kill -TERM $$ ;;
    *) echo "fake-aub"; exit 0 ;;
esac
FAKE
    chmod +x "$fake_bin"

    local cases="$tmp/cases"
    mkdir -p "$cases"
    cat >"$cases/001-timeout-ok.sh" <<'CASE'
CASE_ID="001-timeout-ok"
case_steps() { step --timeout 1 "hang" "$AUB_BIN" hang; }
case_assertions() { assert_timeout 1; }
CASE
    cat >"$cases/002-signal-ok.sh" <<'CASE'
CASE_ID="002-signal-ok"
case_steps() { step "boom" "$AUB_BIN" boom; }
case_assertions() { assert_signal 15 1; }
CASE
    cat >"$cases/003-timeout-fail.sh" <<'CASE'
CASE_ID="003-timeout-fail"
case_steps() { step "ok" "$AUB_BIN"; }
case_assertions() { assert_timeout 1; }
CASE
    cat >"$cases/004-signal-fail.sh" <<'CASE'
CASE_ID="004-signal-fail"
case_steps() { step "ok" "$AUB_BIN"; }
case_assertions() { assert_signal 15 1; }
CASE

    local runs="$tmp/runs"
    AUB_BIN="$fake_bin" CASES_DIR="$cases" RUNS_DIR="$runs" STATE_ROOT="$tmp/state" \
        bash "$0" --state-dir "$tmp/state" --cases-dir "$cases" --runs-dir "$runs" >/dev/null 2>&1
    local rc=$?
    [ "$rc" -eq 1 ] || { echo "self-test: timeout/signal run exited $rc, expected 1" >&2; rm -rf "$tmp"; return 1; }

    local run_dir
    run_dir="$(find "$runs" -mindepth 1 -maxdepth 1 -type d | head -1)"

    grep -q 'timeout:1s' "$run_dir/cases/001-timeout-ok/steps/1-hang/exit.txt" || {
        echo "self-test: a timed-out step lost its exit token" >&2; rm -rf "$tmp"; return 1
    }
    [ -f "$run_dir/cases/001-timeout-ok/steps/1-hang/stdout.bin" ] || {
        echo "self-test: a timed-out step lost its partial stdout artifact" >&2; rm -rf "$tmp"; return 1
    }
    grep -q 'signal:15' "$run_dir/cases/002-signal-ok/steps/1-boom/exit.txt" || {
        echo "self-test: a signalled step lost its exit token" >&2; rm -rf "$tmp"; return 1
    }
    [ -f "$run_dir/cases/002-signal-ok/steps/1-boom/stdout.bin" ] || {
        echo "self-test: a signalled step lost its partial stdout artifact" >&2; rm -rf "$tmp"; return 1
    }

    local id_phase id phase diagnosis
    for id_phase in "003-timeout-fail:timeout" "004-signal-fail:signal"; do
        id="${id_phase%%:*}"
        phase="${id_phase##*:}"
        diagnosis="$run_dir/cases/$id/diagnosis.txt"
        [ -f "$diagnosis" ] || { echo "self-test: $id produced no diagnosis" >&2; rm -rf "$tmp"; return 1; }
        grep -q "phase: $phase" "$diagnosis" || { echo "self-test: $id diagnosis missing phase $phase" >&2; rm -rf "$tmp"; return 1; }
        grep -q "termination_sequence:" "$diagnosis" || { echo "self-test: $id diagnosis missing its termination sequence" >&2; rm -rf "$tmp"; return 1; }
        grep -qF "run_directory: $run_dir" "$diagnosis" || { echo "self-test: $id diagnosis missing the run directory" >&2; rm -rf "$tmp"; return 1; }
    done

    rm -rf "$tmp"
    echo "self-test: timeout and signal reporting ok"
}

# self_test_self_sufficient_build: the runner builds the release binary itself
# when none is supplied, resolving the real path through cargo metadata, so a
# verification environment that runs the suite without a pre-built binary gets
# one built from the repo's own toolchain. A fake cargo shadows the real one so
# the path is proven without a real build.
self_test_self_sufficient_build() {
    local tmp
    tmp="$(mktemp -d)"
    local fake_cargo="$tmp/cargo"
    local fake_target="$tmp/target"
    local fake_bin="$tmp/fake-aub"
    cat >"$fake_bin" <<'FAKE'
#!/usr/bin/env bash
case "${1:-}" in
    status) echo '{"status":"ok"}'; exit 0 ;;
    *) echo "fake-aub"; exit 0 ;;
esac
FAKE
    chmod +x "$fake_bin"
    cat >"$fake_cargo" <<'FAKE'
#!/usr/bin/env bash
case "${1:-}" in
    build)
        mkdir -p "$FAKE_TARGET/release"
        cp "$FAKE_BIN_SRC" "$FAKE_TARGET/release/aub"
        chmod +x "$FAKE_TARGET/release/aub"
        ;;
    metadata)
        printf '{"target_directory":"%s"}' "$FAKE_TARGET"
        ;;
    *) exit 1 ;;
esac
FAKE
    chmod +x "$fake_cargo"

    local cases="$tmp/cases"
    mkdir -p "$cases"
    cat >"$cases/001-status.sh" <<'CASE'
CASE_ID="001-status"
case_steps() { step "status" "$AUB_BIN" status; }
case_assertions() { assert_exit 0 1; assert_json_field 1 status ok; }
CASE

    local runs="$tmp/runs"
    local out
    out="$(PATH="$tmp:$PATH" FAKE_TARGET="$fake_target" FAKE_BIN_SRC="$fake_bin" \
        AUB_BIN="$tmp/no-such-aub" CASES_DIR="$cases" RUNS_DIR="$runs" STATE_ROOT="$tmp/state" \
        bash "$0" --state-dir "$tmp/state" --cases-dir "$cases" --runs-dir "$runs" 2>&1)"
    local rc=$?
    [ "$rc" -eq 0 ] || { echo "self-test: self-sufficient build run exited $rc: $out" >&2; rm -rf "$tmp"; return 1; }

    [ -x "$fake_target/release/aub" ] || { echo "self-test: the runner did not build the binary" >&2; rm -rf "$tmp"; return 1; }
    grep -q '"verdict": "pass"' "$runs"/*/summary.json || { echo "self-test: the built binary did not run the suite" >&2; rm -rf "$tmp"; return 1; }

    rm -rf "$tmp"
    echo "self-test: self-sufficient build ok"
}

self_test() {
    local overall=0
    self_test_basic || overall=1
    self_test_pruning || overall=1
    self_test_golden || overall=1
    self_test_consistency || overall=1
    self_test_summary_parseback || overall=1
    self_test_timeout_and_signal || overall=1
    self_test_self_sufficient_build || overall=1
    [ "$overall" -eq 0 ] && echo "self-test: ok"
    return "$overall"
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

    # The runner is self-sufficient: a verification environment that runs the
    # suite without a pre-built binary gets one built here, from the repo's own
    # toolchain, so the gate cannot fail on a missing artifact. The build is
    # skipped when the caller supplied a working binary (bin/checks/60-e2e
    # pre-builds and passes AUB_BIN).
    if [ ! -x "$AUB_BIN" ]; then
        echo "run.sh: release binary not found at $AUB_BIN; building it (cargo build --release)" >&2
        cargo build --release || die "cargo build --release failed"
        # Ask cargo where it actually put the binary: CARGO_TARGET_DIR and
        # ~/.cargo/config.toml can both move it away from target/release.
        target_dir="$(cargo metadata --format-version 1 --no-deps | jq -r '.target_directory')"
        AUB_BIN="$target_dir/release/aub"
    fi
    [ -x "$AUB_BIN" ] || die "release binary not found at $AUB_BIN after building"

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
    write_manifest
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
