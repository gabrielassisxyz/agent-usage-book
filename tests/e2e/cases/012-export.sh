# aub-xus.7: the export joins the external friction ledger on the run
# identifier with no additional transformation, run against the release binary
# under the end-to-end runner, because the property is about the command's
# contract with a consumer that lives outside this database. The ledger fixture
# carries exactly what aub does not own: wall time, retries and subjective run
# quality. The join matches the export's key column against the ledger's
# run_id column, byte for byte, and nothing else.

CASE_ID="012-export"
CASE_DESCRIPTION="The export emits versioned JSONL keyed by session or run; logical identifiers ride only behind the explicit flag; a re-run differs only in generated_at; the friction ledger joins on the run identifier alone."

# The state paths resolve inside case_preconditions: the runner sources this
# file before the fresh state directory exists.
LEDGER_DB=""
FRICTION_LEDGER=""
EXPORT_RUN_1=""
EXPORT_RUN_2=""
EXPORT_JOIN=""

aub_export() {
    env \
        "HOME=$STATE_DIR/home" \
        "AUB_STATE_DIR=$STATE_DIR/aub" \
        "$AUB_BIN" export "$@"
}

case_preconditions() {
    require_command "$AUB_BIN"
    require_command sqlite3
    require_command jq
    require_command join

    LEDGER_DB="$STATE_DIR/aub/ledger.db"
    FRICTION_LEDGER="$STATE_DIR/friction-ledger/runs.tsv"
    EXPORT_RUN_1="$STATE_DIR/export-run-1.jsonl"
    EXPORT_RUN_2="$STATE_DIR/export-run-2.jsonl"
    EXPORT_JOIN="$STATE_DIR/joined.tsv"

    # The external friction ledger, in its own shape: the run identifier it
    # shares with aub, plus the wall-time and survey data aub deliberately
    # does not absorb (PLAN.md section 5).
    mkdir -p "$(dirname "$FRICTION_LEDGER")"
    printf 'run_id\twall_seconds\tretries\tquality\n' >"$FRICTION_LEDGER"
    printf 'run-friction-1\t3600\t2\thuman-flagged-slow\n' >>"$FRICTION_LEDGER"
    printf 'run-friction-2\t900\t0\tok\n' >>"$FRICTION_LEDGER"
}

case_steps() {
    # The first export runs against an empty ledger: it creates and migrates
    # the database through the real command path and emits only the header.
    step "first export on an empty ledger" aub_export --key run-id

    # Seed through the schema the release binary just migrated: two sessions
    # sharing one run id across two sources, one run-less session, and usage
    # events with the transcript paths the real ingest writes.
    step "seed the ledger fixture" sqlite3 "$LEDGER_DB" "
        INSERT INTO session (source, native_session_id, run_id, project_key, repository_key, start, end)
        VALUES ('claude-code','sess-a','run-friction-1','proj-alpha','repo-alpha',100,200),
               ('codex','sess-b','run-friction-1','proj-alpha','repo-alpha',150,300),
               ('claude-code','sess-c',NULL,'proj-beta','repo-beta',10,20);
        INSERT INTO usage_event (canonical_event_id, session_id, evidence_kind, source_provenance, parser_version, created_at)
        VALUES ('ce-1','sess-a','transcript','/home/nobody/.claude/projects/p/sess-a.jsonl','v1',100),
               ('ce-2','sess-b','transcript','/home/nobody/.codex/sess-b.jsonl','v1',150),
               ('ce-3','sess-c','transcript','/home/nobody/.claude/projects/p/sess-c.jsonl','v1',10);
        INSERT INTO usage_component (event_id, token_class, count)
        VALUES (1,'input',100),(1,'output',40),(2,'input',200),(2,'output',60),(3,'input',7);
    "

    step "run-keyed export with logical ids" aub_export --key run-id --include-logical-ids
    step "session-keyed export without logical ids" aub_export --key session-id
    step "re-run the run-keyed export" aub_export --key run-id --include-logical-ids

    # Determinism artifacts: the first and second run-keyed exports, unchanged.
    cp "$(step_dir 3)/stdout.bin" "$EXPORT_RUN_1"
    cp "$(step_dir 5)/stdout.bin" "$EXPORT_RUN_2"

    # The join: export rows and ledger rows matched on the run identifier
    # alone. The identifier crosses the boundary verbatim; no transformation,
    # no reformatting, no key mapping.
    step "join the friction ledger to the export" bash -c '
        set -eu
        join -t "$(printf "\t")" \
            <(tail -n +2 "$1" | sort -k1,1) \
            <(jq -r "select(.schema == null) | [.key, ([.usage[] | .value | tonumber] | add)] | @tsv" "$2" | sort -k1,1)
    ' _ "$FRICTION_LEDGER" "$EXPORT_RUN_1"
}

case_assertions() {
    # The empty-ledger export is one versioned header line, nothing else.
    assert_exit 0 1
    assert_stdout_contains 1 '"schema":1'
    assert_stdout_contains 1 '"key":"run-id"'

    # The seeding step wrote real rows.
    assert_exit 0 2

    # The run-keyed export aggregates the two sessions that share the run:
    # usage 300 input + 100 output, two sessions, both project keys listed
    # because the flag asked for them, and the transcript paths stayed behind.
    assert_exit 0 3
    assert_stdout_contains 3 '"key":"run-friction-1"'
    assert_stdout_contains 3 '"value":"300","unit":"tokens"'
    assert_stdout_contains 3 '"value":"100","unit":"tokens"'
    assert_stdout_contains 3 '"value":"2","unit":"sessions"'
    assert_stdout_contains 3 '"project_keys":["proj-alpha"]'
    assert_stdout_contains 3 '"ledger_generation":0'
    assert_stdout_contains 3 '"ingestion_generation":0'
    assert_stdout_contains 3 '"included_identifiers":["project","repository"]'
    if grep -qF "/home/nobody" "$(step_dir 3)/stdout.bin"; then
        record_assertion "no absolute machine path in the export" "absent" "present" "fail"
        CASE_FAILED=1
    else
        record_assertion "no absolute machine path in the export" "absent" "absent" "pass"
    fi

    # The session-keyed export without the flag: every session, no logical
    # identifiers anywhere, and the header says the same.
    assert_exit 0 4
    assert_stdout_contains 4 '"key":"claude-code:sess-a"'
    assert_stdout_contains 4 '"key":"codex:sess-b"'
    assert_stdout_contains 4 '"key":"claude-code:sess-c"'
    assert_stdout_contains 4 '"included_identifiers":[]'
    if grep -qE "proj-alpha|repo-alpha" "$(step_dir 4)/stdout.bin"; then
        record_assertion "logical ids absent without the flag" "absent" "present" "fail"
        CASE_FAILED=1
    else
        record_assertion "logical ids absent without the flag" "absent" "absent" "pass"
    fi

    # Determinism: the second run-keyed export is byte-identical to the first
    # except for generated_at, and generated_at actually moved, so the
    # normalization is load-bearing rather than papering over two equals.
    assert_exit 0 5
    if cmp -s "$EXPORT_RUN_1" "$EXPORT_RUN_2"; then
        record_assertion "generated_at is volatile between runs" "differs" "identical" "fail"
        CASE_FAILED=1
    else
        record_assertion "generated_at is volatile between runs" "differs" "differs" "pass"
    fi
    if diff -q \
        <(sed 's/"generated_at":[0-9]*/"generated_at":NORMALIZED/' "$EXPORT_RUN_1") \
        <(sed 's/"generated_at":[0-9]*/"generated_at":NORMALIZED/' "$EXPORT_RUN_2") \
        >/dev/null; then
        record_assertion "re-run identical after normalizing generated_at" "identical" "identical" "pass"
    else
        record_assertion "re-run identical after normalizing generated_at" "identical" "differs" "fail"
        CASE_FAILED=1
    fi

    # The join: exactly the one run the two sources share, matched on the
    # identifier alone. The joined line carries the ledger's wall time and the
    # export's token total side by side, which is the whole integration.
    assert_exit 0 6
    assert_stdout_contains 6 "run-friction-1"
    assert_stdout_contains 6 "3600"
    assert_stdout_contains 6 "human-flagged-slow"
    assert_stdout_contains 6 "400"
    # The planted negative: a ledger run with no export row must not appear in
    # an inner join, so a naive join that echoes ledger rows fails here.
    if grep -qF "run-friction-2" "$(step_dir 6)/stdout.bin"; then
        record_assertion "ledger runs without an export row do not join" "absent" "present" "fail"
        CASE_FAILED=1
    else
        record_assertion "ledger runs without an export row do not join" "absent" "absent" "pass"
    fi
}