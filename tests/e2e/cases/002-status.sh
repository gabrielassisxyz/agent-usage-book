# The status command exits zero and, at raised verbosity, emits a structured
# diagnostic event on stderr.

CASE_ID="002-status"
CASE_DESCRIPTION="The status command exits zero and emits a structured diagnostic event at raised verbosity."

case_steps() {
    step "status" "$AUB_BIN" -v status
}

case_assertions() {
    assert_exit 0 1
    assert_stderr_contains 1 "run_started"
}
