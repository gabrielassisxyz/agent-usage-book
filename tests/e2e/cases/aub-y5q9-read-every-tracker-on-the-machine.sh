# aub-y5q9: `aub task ingest` reads every configured tracker in one run, each
# under its own source name.
#
# Two tracker databases whose `events.id` sequences both start at 1 and which
# both carry the native id `aub-1` are the collision that ended one-namespace
# ingestion: under one shared name the second tracker's event ids collide on
# `(tracker_source, tracker_event_id)` and its rows are silently dropped, and
# the shared native id conflates the two tasks into one identity. Each entry
# under `[[trackers]]` carries the `name` its events are keyed under, so both
# histories land side by side, a re-run over unchanged trackers moves nothing,
# and the per-tracker counts are reported in both output formats. A tracker
# whose `beads.db` is missing is reported as failed for that tracker while a
# healthy tracker in the same run still ingests; the command's exit class is
# then that failure's (class 5, the class opening the database produces).

CASE_ID="aub-y5q9-read-every-tracker-on-the-machine"
CASE_DESCRIPTION="task ingest reads every configured tracker under its own source name, with per-tracker counts, idempotent re-runs, the config listing and the partial-failure exit class."

CONFIG=""
CONFIG_PARTIAL=""
LEDGER_DB=""

case_preconditions() {
    require_command "$AUB_BIN"
    require_command sqlite3

    # Two trackers, each with an events table whose ids start at 1 and whose
    # only native id is the same string. Under one namespace these collide;
    # under per-tracker names they cannot.
    for repo in repo-a repo-b repo-c; do
        mkdir -p "$STATE_DIR/$repo"
        sqlite3 "$STATE_DIR/$repo/beads.db" <<SQL
CREATE TABLE events (
    id INTEGER PRIMARY KEY,
    issue_id TEXT NOT NULL,
    event_type TEXT NOT NULL,
    actor TEXT,
    old_value TEXT,
    new_value TEXT,
    created_at TEXT NOT NULL
);
INSERT INTO events (id, issue_id, event_type, actor, old_value, new_value, created_at)
VALUES (1, 'aub-1', 'status_changed', 'agent-1', 'open', 'in_progress', '2026-08-25T10:00:00Z'),
       (2, 'aub-1', 'status_changed', 'agent-1', 'in_progress', 'closed', '2026-08-25T11:00:00Z');
SQL
    done
    # repo-d holds a tracker directory with no database in it.
    mkdir -p "$STATE_DIR/repo-d"

    LEDGER_DB="$STATE_DIR/ledger.db"
    CONFIG="$STATE_DIR/aub.toml"
    cat > "$CONFIG" <<EOT
state.dir = "$STATE_DIR"

[[trackers]]
name = "repo-a"
kind = "local"
path = "$STATE_DIR/repo-a"

[[trackers]]
name = "repo-b"
kind = "local"
path = "$STATE_DIR/repo-b"
EOT

    # The partial-failure run reads a second configuration: repo-c's database
    # is healthy, repo-d's is the empty directory above.
    CONFIG_PARTIAL="$STATE_DIR/aub-partial.toml"
    cat > "$CONFIG_PARTIAL" <<EOT
state.dir = "$STATE_DIR"

[[trackers]]
name = "repo-c"
kind = "local"
path = "$STATE_DIR/repo-c"

[[trackers]]
name = "repo-d"
kind = "local"
path = "$STATE_DIR/repo-d"
EOT
}

case_steps() {
    step "task ingest" env \
        "HOME=$STATE_DIR/home" \
        "AUB_CONFIG_FILE=$CONFIG" \
        "$AUB_BIN" task ingest
    step "task ingest again" env \
        "HOME=$STATE_DIR/home" \
        "AUB_CONFIG_FILE=$CONFIG" \
        "$AUB_BIN" task ingest
    step "task ingest json" env \
        "HOME=$STATE_DIR/home" \
        "AUB_CONFIG_FILE=$CONFIG" \
        "$AUB_BIN" task ingest --format json
    step "task event rows" sqlite3 "$LEDGER_DB" "
        SELECT COUNT(*) FROM task_event;
        SELECT task_source, COUNT(*) FROM task_event GROUP BY task_source;
        SELECT COUNT(DISTINCT task_source) FROM task_event WHERE task_native = 'aub-1';
    "
    step "config" env \
        "HOME=$STATE_DIR/home" \
        "AUB_CONFIG_FILE=$CONFIG" \
        "$AUB_BIN" config
    step "task ingest with one tracker missing" env \
        "HOME=$STATE_DIR/home" \
        "AUB_CONFIG_FILE=$CONFIG_PARTIAL" \
        "$AUB_BIN" task ingest
}

case_assertions() {
    # Both trackers ingest in one run, each named, two events each.
    assert_exit 0 1
    assert_stdout_contains 1 "task ingest repo-a: events_inserted=2 events_already_present=0 quarantines_inserted=0 quarantines_already_present=0"
    assert_stdout_contains 1 "task ingest repo-b: events_inserted=2 events_already_present=0 quarantines_inserted=0 quarantines_already_present=0"

    # A re-run over unchanged trackers lands nothing and leaves the event
    # counts identical: the summaries below agree with the row count asserted
    # in step 4, which stays 4.
    assert_exit 0 2
    assert_stdout_contains 2 "task ingest repo-a: events_inserted=0 events_already_present=2 quarantines_inserted=0 quarantines_already_present=0"
    assert_stdout_contains 2 "task ingest repo-b: events_inserted=0 events_already_present=2 quarantines_inserted=0 quarantines_already_present=0"

    # The JSON document carries the run's totals plus one entry per tracker.
    assert_exit 0 3
    assert_json_field 3 events_inserted 0
    assert_json_field 3 events_already_present 4
    assert_stdout_contains 3 '"name":"repo-a","events_inserted":0,"events_already_present":2,"quarantines_inserted":0,"quarantines_already_present":0'
    assert_stdout_contains 3 '"name":"repo-b","events_inserted":0,"events_already_present":2,"quarantines_inserted":0,"quarantines_already_present":0'

    # Four event rows: the same upstream ids from two trackers under two
    # names, two identities for the shared native id.
    assert_exit 0 4
    assert_stdout_contains 4 "4"
    assert_stdout_contains 4 "repo-a|2"
    assert_stdout_contains 4 "repo-b|2"
    assert_stdout_contains 4 "2"

    # The config listing shows one line per tracker, under its own name.
    assert_exit 0 5
    assert_stdout_contains 5 "trackers"
    assert_stdout_contains 5 "repo-a"
    assert_stdout_contains 5 "repo-b"

    # The partial run: repo-d's database does not exist, so it is reported as
    # failed for that tracker while repo-c still ingests its events; the
    # command's exit class is the failure's (5, the unopenable database), and
    # the stderr line names the run's incomplete state.
    assert_exit 5 6
    assert_stdout_contains 6 "task ingest repo-c: events_inserted=2 events_already_present=0 quarantines_inserted=0 quarantines_already_present=0"
    assert_stdout_contains 6 "task ingest repo-d: failed: "
    assert_stderr_contains 6 "task ingest: 1 of 2 configured trackers failed:"
}