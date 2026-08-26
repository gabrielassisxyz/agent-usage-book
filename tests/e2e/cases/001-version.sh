# The binary prints its version and exits zero with no arguments.

CASE_ID="001-version"
CASE_DESCRIPTION="The binary prints its version and exits zero with no arguments."

case_preconditions() {
    require_command "$AUB_BIN"
}

case_steps() {
    step "print version" "$AUB_BIN"
}

case_assertions() {
    assert_exit 0 1
    assert_stdout_contains 1 "aub"
}
