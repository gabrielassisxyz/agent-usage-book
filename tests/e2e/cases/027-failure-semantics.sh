# The failure semantics matrix: full invocation exercises all nine stable exit classes.
# Plan section 30 failure semantics cases asserting exit classes and structured envelopes.

CASE_ID="027-failure-semantics"
CASE_DESCRIPTION="The failure semantics matrix composed invocation exercises all nine stable exit classes."

case_steps() {
    # ExitClass::Success (0)
    step "class 0 success" "$AUB_BIN" status
    # ExitClass::Internal (1)
    step "class 1 internal" "$AUB_BIN" __exit-class 1
    # ExitClass::Usage (2)
    step "class 2 usage" "$AUB_BIN" spend --format=json --since not-a-date
    # ExitClass::AuthRequired (3)
    step "class 3 auth required" "$AUB_BIN" __exit-class 3
    # ExitClass::RemoteUnavailable (4)
    step "class 4 remote unavailable" "$AUB_BIN" __exit-class 4
    # ExitClass::Store (5)
    step "class 5 store" "$AUB_BIN" __exit-class 5
    # ExitClass::InsufficientEvidence (6)
    step "class 6 insufficient evidence" "$AUB_BIN" __exit-class 6
    # ExitClass::ThresholdNotMet (7)
    step "class 7 threshold not met" "$AUB_BIN" __exit-class 7
    # ExitClass::IngestIncomplete (8)
    step "class 8 ingest incomplete" "$AUB_BIN" __exit-class 8
}

case_assertions() {
    assert_exit 0 1
    assert_exit 1 2
    assert_exit 2 3
    assert_json_field 3 error.exit_class 2
    assert_json_field 3 error.code INVALID_USAGE
    assert_exit 3 4
    assert_exit 4 5
    assert_exit 5 6
    assert_exit 6 7
    assert_exit 7 8
    assert_exit 8 9
}
