# Isolation: a case starts with a fresh state directory and cannot observe
# another case's state. This case deliberately looks and finds nothing.

CASE_ID="004-isolation"
CASE_DESCRIPTION="A case starts with a fresh state directory and cannot observe another case's state."

case_steps() {
    step "list state" sh -c 'find "$1" -mindepth 1 | wc -l' _ "$STATE_DIR"
    step "write marker" sh -c 'echo marker > "$1/marker.txt"' _ "$STATE_DIR"
    step "list state again" sh -c 'find "$1" -mindepth 1 | wc -l' _ "$STATE_DIR"
}

case_assertions() {
    # The state directory is empty before the case writes anything.
    assert_stdout_contains 1 "0"
    # After writing one marker, exactly one file exists.
    assert_stdout_contains 3 "1"
}
