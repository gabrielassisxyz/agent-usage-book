# A command that fails under --format json prints the versioned error envelope
# on stdout, carrying the stable symbolic problem code and the numeric exit
# class, so automation reads a name instead of parsing the prose line or
# inferring the class from the process exit code. `aub spend` rejects a
# malformed --since before it resolves any configuration. Without --format json
# the same failure stays a plain stderr line.

CASE_ID="010-json-error-envelope"
CASE_DESCRIPTION="A --format json failure prints the problem-code error envelope on stdout."

case_steps() {
    step "json error envelope" "$AUB_BIN" spend --format=json --since not-a-date
    step "text error line" "$AUB_BIN" spend --since not-a-date
}

case_assertions() {
    # assert_exit CODE STEP
    assert_exit 2 1
    assert_json_field 1 schema 2
    assert_json_field 1 command spend
    assert_json_field 1 error.code INVALID_USAGE
    assert_json_field 1 error.exit_class 2

    assert_exit 2 2
    assert_stderr_contains 2 "aub: "
}
