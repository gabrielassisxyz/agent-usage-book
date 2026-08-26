# The exit-class contract is observable through a real subprocess: an unknown
# argument is a usage error (2), and the test hook exposes the other classes.

CASE_ID="003-exit-classes"
CASE_DESCRIPTION="The exit-class contract is observable through a real subprocess."

case_steps() {
    step "unknown flag" "$AUB_BIN" --definitely-not-a-flag
    step "auth required" "$AUB_BIN" __exit-class 3
    step "remote unavailable" "$AUB_BIN" __exit-class 4
}

case_assertions() {
    assert_exit 2 1
    assert_exit 3 2
    assert_exit 4 3
}
