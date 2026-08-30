# The binary explains itself: help lists the shipping commands, a usage error names
# the argument it rejected on stderr, and a command that rejects --format says why.

CASE_ID="008-command-surface"
CASE_DESCRIPTION="help, version and usage errors are visible, not silent exits."

case_steps() {
    step "help" "$AUB_BIN" --help
    step "version" "$AUB_BIN" --version
    step "unknown argument" "$AUB_BIN" --definitely-not-a-flag
    step "format refused" "$AUB_BIN" config --format json
}

case_assertions() {
    assert_exit 0 1
    assert_stdout_contains 1 "status"
    assert_stdout_contains 1 "spend"
    assert_stdout_contains 1 "config"
    assert_exit 0 2
    assert_stdout_contains 2 "aub "
    assert_exit 2 3
    assert_stderr_contains 3 "aub: unknown argument: --definitely-not-a-flag"
    assert_exit 2 4
    assert_stderr_contains 4 "config prints provenance, not a report"
}
