# aub-wyu.1: the rate book becomes immutable versioned records through the
# import command, run against the release binary under the end-to-end runner,
# because the property is about the command's contract: a re-import is a
# visible no-op, the effective book resolves to what is true today, and
# history keeps superseded records with their intervals. The book fixture is
# the initial dated rate card (PLAN.md section 32): the existing price table
# with its date comments become structured metadata.

CASE_ID="011-rate-card"
CASE_DESCRIPTION="The rate book imports as immutable dated records; re-import is a visible no-op; show resolves the effective book; history keeps superseded records; a missing book is refused by name."

RATE_BOOK="$REPO_ROOT/tests/fixtures/rate-book/rates.toml"

aub_rate_card() {
    env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR/aub" \
        "$AUB_BIN" rate-card "$@"
}

case_preconditions() {
    require_command "$AUB_BIN"
    if [ ! -r "$RATE_BOOK" ]; then
        record_assertion "rate book fixture readable" "present" "absent" "fail"
        CASE_FAILED=1
    fi
}

case_steps() {
    step "import the existing price table" \
        aub_rate_card import "$RATE_BOOK"
    step "re-import the same book" \
        aub_rate_card import "$RATE_BOOK"
    step "show the effective book" \
        aub_rate_card show
    step "history keeps every record" \
        aub_rate_card history
    step "a missing book is refused by name" \
        aub_rate_card import "$STATE_DIR/absent-book.toml"
}

case_assertions() {
    # The first import adds the full book and reports the counts.
    assert_exit 0 1
    assert_stdout_contains 1 "added=55"
    assert_stdout_contains 1 "unchanged=0"

    # The re-import adds nothing and says so: idempotence is visible, not
    # inferred from an unchanged digest.
    assert_exit 0 2
    assert_stdout_contains 2 "added=0"
    assert_stdout_contains 2 "unchanged=55"

    # show resolves what is true today. The book's earliest interval starts
    # 2026-06-24 and no shipped row is expired, so the effective book is the
    # whole table: spot-check one Anthropic row and the OpenAI rows, and the
    # publication reference arriving as data.
    assert_exit 0 3
    assert_stdout_contains 3 "claude-fable-5"
    assert_stdout_contains 3 "input 10.00 USD per_million_tokens 2026-06-24-open"
    assert_stdout_contains 3 "gpt-5.6"
    assert_stdout_contains 3 "claude-api reference"

    # history lists every record including the introductory rows with their
    # intervals and review-due policy.
    assert_exit 0 4
    assert_stdout_contains 4 "2026-06-24-2026-08-31"
    assert_stdout_contains 4 "review-due 2026-08-31"

    # The missing book names the path and exits with the ingest class.
    assert_exit 8 5
    assert_stderr_contains 5 "absent-book.toml"
}