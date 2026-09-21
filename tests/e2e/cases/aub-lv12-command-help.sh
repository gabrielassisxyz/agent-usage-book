# aub-lv12: `--help` / `-h` after a command name prints that command's usage
# and runs nothing. `aub backup --help` once wrote a ledger archive into a
# directory named `--help` in the caller's working directory. Every call here
# runs inside a scratch directory with XDG_STATE_HOME and AUB_STATE_DIR pointed
# into it, and the scratch listing is compared before and after.

CASE_ID="aub-lv12-command-help"
CASE_DESCRIPTION="Every command's --help and -h print its usage, exit zero and write nothing; dash-prefixed written paths are refused by name."

SCRATCH=""

# listing: every path under the scratch directory with its content digest, so
# a new empty directory shows up as well as a new or changed file.
listing() {
    (cd "$SCRATCH" && find . -mindepth 1 -printf '%y %p\n' | sort && find . -type f -print0 | sort -z | xargs -0 -r sha256sum)
}

in_scratch() {
    (cd "$SCRATCH" && env \
        "HOME=$SCRATCH/home" \
        "XDG_STATE_HOME=$SCRATCH/xdg-state" \
        "AUB_STATE_DIR=$SCRATCH/xdg-state/aub" \
        "$@")
}

case_preconditions() {
    require_command "$AUB_BIN"
    require_command find
    require_command sha256sum
    SCRATCH="$STATE_DIR/scratch"
    mkdir -p "$SCRATCH/home" "$SCRATCH/xdg-state"
}

# help_sweep: for every command on the surface, both help spellings and the
# `aub help <command>` form must exit 0, open with `aub <command>`, agree with
# each other, and leave the scratch directory unchanged. Prints one line per
# failure and a final count.
help_sweep() {
    local surface="$1" before after command flag out ref failures=0 checked=0
    before="$(listing)"
    while IFS= read -r command; do
        case "$command" in ''|\#*) continue ;; esac
        ref="$(in_scratch "$AUB_BIN" help "$command")" || { echo "FAIL help $command exited non-zero"; failures=$((failures + 1)); }
        for flag in --help -h; do
            checked=$((checked + 1))
            if ! out="$(in_scratch "$AUB_BIN" "$command" "$flag")"; then
                echo "FAIL $command $flag exited non-zero"
                failures=$((failures + 1))
                continue
            fi
            case "$(printf '%s\n' "$out" | head -n1)" in
                "aub $command"|"aub $command "*) ;;
                *) echo "FAIL $command $flag first line: $(printf '%s\n' "$out" | head -n1)"; failures=$((failures + 1)) ;;
            esac
            [ "$out" = "$ref" ] || { echo "FAIL $command $flag differs from aub help $command"; failures=$((failures + 1)); }
        done
    done <"$surface"
    after="$(listing)"
    [ "$before" = "$after" ] || { echo "FAIL scratch directory changed"; failures=$((failures + 1)); }
    echo "sweep checked=$checked failures=$failures"
}

# unchanged_after CMD...: runs CMD in the scratch directory, then reports
# whether the scratch listing changed, keeping CMD's own exit status.
unchanged_after() {
    local before after rc
    before="$(listing)"
    in_scratch "$@"
    rc=$?
    after="$(listing)"
    if [ "$before" = "$after" ]; then echo "scratch unchanged"; else echo "scratch CHANGED"; fi
    return "$rc"
}

case_steps() {
    step "help sweep over the command surface" help_sweep "$E2E_DIR/command-surface.txt"
    step "backup --help" unchanged_after "$AUB_BIN" backup --help
    step "backup restore --help" unchanged_after "$AUB_BIN" backup restore a.tar --help
    step "drill --archive --help" unchanged_after "$AUB_BIN" drill --archive a.tar --help
    step "backup dash destination" unchanged_after "$AUB_BIN" backup -x
    step "restore dash archive" unchanged_after "$AUB_BIN" backup restore -a b
    step "restore dash dest" unchanged_after "$AUB_BIN" backup restore a -b
    step "aub help" "$AUB_BIN" help
    # An empty run-keyed export creates and migrates the ledger, the cheapest
    # way to give the backup below something to archive.
    step "create the ledger" in_scratch "$AUB_BIN" export --key run-id
    step "backup dot-slash dash destination" in_scratch "$AUB_BIN" backup ./-dir
    step "dot-slash destination exists" test -d "$SCRATCH/-dir"
}

case_assertions() {
    assert_exit 0 1
    assert_stdout_contains 1 "failures=0"
    assert_stdout_matches 1 "^sweep checked=[1-9][0-9]* failures=0$"

    assert_exit 0 2
    assert_stdout_contains 2 "aub backup [--scheduled] [DESTINATION]"
    assert_stdout_contains 2 "scratch unchanged"
    assert_exit 0 3
    assert_stdout_contains 3 "scratch unchanged"
    assert_exit 0 4
    assert_stdout_contains 4 "aub drill"
    assert_stdout_contains 4 "scratch unchanged"

    assert_exit 2 5
    assert_stderr_contains 5 "unknown argument: -x"
    assert_stdout_contains 5 "scratch unchanged"
    assert_exit 2 6
    assert_stderr_contains 6 "unknown argument: -a"
    assert_stdout_contains 6 "scratch unchanged"
    assert_exit 2 7
    assert_stderr_contains 7 "unknown argument: -b"
    assert_stdout_contains 7 "scratch unchanged"

    assert_exit 0 8
    assert_stdout_contains 8 "commands:"

    assert_exit 0 9
    assert_exit 0 10
    assert_exit 0 11
}
