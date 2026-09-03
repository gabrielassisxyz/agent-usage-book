# `aub ingest transcripts` then `aub rebuild transcripts` then `aub ingest
# transcripts` over one seeded corpus: both entry points report identical
# canonical event counts, and the rebuild deletes only rebuildable tables while
# the ingestion generation advances across the sweep instead of resetting.

CASE_ID="013-ingest-rebuild"
CASE_DESCRIPTION="rebuild transcripts followed by ingest reproduces identical canonical event counts."

CONFIG=""

case_preconditions() {
    local corpus="$STATE_DIR/transcripts/claude-code"
    mkdir -p "$corpus"

    # One file, three parseable records and one malformed record: m1 carries
    # all four known components, m2 and m3 carry input and output, and the
    # malformed record quarantines. The counts below are exact because the
    # corpus is fixed.
    cat > "$corpus/session.jsonl" <<'JSONL'
{"type":"assistant","timestamp":"2026-08-25T10:00:00.000Z","sessionId":"s1","message":{"id":"m1","usage":{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":20,"cache_creation_input_tokens":10}}}
{"type":"assistant","timestamp":"2026-08-25T10:05:00.000Z","sessionId":"s1","message":{"id":"m2","usage":{"input_tokens":30,"output_tokens":12,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}
{"type":"assistant","timestamp":"2026-08-25T11:00:00.000Z","sessionId":"s2","message":{"id":"m3","usage":{"input_tokens":7,"output_tokens":2,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}
{"type":"assistant","timestamp":"2026-08-25T10:06:00.000Z","sessionId":"s1","message":{"id":"m9","usage":{"input_tokens":"wrong-type","output_tokens":5}}}
JSONL

    CONFIG="$STATE_DIR/aub.toml"
    cat > "$CONFIG" <<EOT
[[transcripts]]
name = "claude-code"
root = "$corpus"
pattern = "**/*.jsonl"
format = "claude-code"
EOT
}

case_steps() {
    step "ingest first" env \
        "HOME=$STATE_DIR/home" \
        "AUB_CONFIG_FILE=$CONFIG" \
        "$AUB_BIN" ingest transcripts
    step "rebuild transcripts" env \
        "HOME=$STATE_DIR/home" \
        "AUB_CONFIG_FILE=$CONFIG" \
        "$AUB_BIN" rebuild transcripts
    step "ingest again" env \
        "HOME=$STATE_DIR/home" \
        "AUB_CONFIG_FILE=$CONFIG" \
        "$AUB_BIN" ingest transcripts
}

case_assertions() {
    # Step 1: the first ingest lands the whole corpus as generation 1.
    assert_exit 0 1
    assert_stdout_contains 1 "sources=claude-code scanned=1 parsed=1 skipped=0 unreadable=0 quarantined=1 generation=1"
    assert_stdout_contains 1 "events: written=3 already-ingested=0"
    assert_stdout_contains 1 "occurrences: written=3 already-ingested=0"
    assert_stdout_contains 1 "components=8 sessions=2 replaced=0"

    # Step 2: the sweep's printed table list is the golden, so the case declares
    # its criterion rendering: the report names exactly the six tables the
    # transcripts group owns, in the order the shared taxonomy derives them, and
    # nothing else. A meter, observation, calibration or rate-card table cannot
    # appear without the golden failing, because the group carries no such class.
    assert_exit 0 2
    assert_golden 2 "$REPO_ROOT/tests/e2e/golden-013-rebuild-transcripts.txt"

    # Step 3: re-ingest onto the emptied tables reproduces the first pass's
    # counts exactly: three canonical events, three occurrences, eight
    # components, two sessions, one quarantine record, and the generation
    # advanced to 2 rather than reset.
    assert_exit 0 3
    assert_stdout_contains 3 "sources=claude-code scanned=1 parsed=1 skipped=0 unreadable=0 quarantined=1 generation=2"
    assert_stdout_contains 3 "events: written=3 already-ingested=0"
    assert_stdout_contains 3 "occurrences: written=3 already-ingested=0"
    assert_stdout_contains 3 "components=8 sessions=2 replaced=0"
}