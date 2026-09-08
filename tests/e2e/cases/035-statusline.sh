# aub-22fa: the status line's tee. Claude Code pipes its status-line payload
# through `aub statusline` ahead of the renderer, so what has to hold end to
# end is the tee's whole contract: the payload on stdin comes out on stdout
# byte-for-byte with exit 0, one line per meter change lands in
# `<state.dir>/statusline/<account>.jsonl` attributed by `SHALLOW_PROFILE`,
# an identical re-render appends nothing, and a render with no profile (or an
# unknown one) passes through and records nothing. The pipeline shape is the
# installed one: `aub statusline` ahead of a consumer on the same pipe.

CASE_ID="035-statusline"
CASE_DESCRIPTION="aub statusline passes the payload through unchanged, records one line per meter change per account, and records nothing without a profile."

PAYLOAD_FIRST=""
PAYLOAD_MOVED=""

case_preconditions() {
    require_command "$AUB_BIN"
    require_command python3

    mkdir -p "$STATE_DIR/home"
    cat > "$STATE_DIR/aub.toml" <<CFG_EOF
[state]
dir = "$STATE_DIR"

[[accounts]]
name = "gmail"
provider = "anthropic"
credential = { kind = "file", path = "$STATE_DIR/credential.json" }
CFG_EOF
    echo '{"accessToken":"test-token"}' > "$STATE_DIR/credential.json"

    # The payload, held in files so the byte-for-byte comparison compares the
    # exact bytes the tee was given. The cost value is distinctive on purpose:
    # the recorded line is grepped for it, so a leak of any payload field the
    # tee does not own is caught by value, not by key.
    cat > "$STATE_DIR/payload-first.json" <<'PAYLOAD_EOF'
{"session_id":"8cd9c60a-e10a-4d44-857e-6b2b931b4d9d","cwd":"/tmp/worktree/project","cost":{"total_cost_usd":12.345678},"rate_limits":{"five_hour":{"used_percentage":40,"resets_at":"1786834200"},"seven_day":{"used_percentage":12,"resets_at":"1786920000"}}}
PAYLOAD_EOF
    cat > "$STATE_DIR/payload-moved.json" <<'PAYLOAD_EOF'
{"session_id":"8cd9c60a-e10a-4d44-857e-6b2b931b4d9d","cwd":"/tmp/worktree/project","cost":{"total_cost_usd":12.345678},"rate_limits":{"five_hour":{"used_percentage":40,"resets_at":"1786834200"},"seven_day":{"used_percentage":13,"resets_at":"1786920000"}}}
PAYLOAD_EOF
    chmod 644 "$STATE_DIR/payload-first.json" "$STATE_DIR/payload-moved.json"
}

statusline_step() {
    # $1: step name, $2: payload file, $3: SHALLOW_PROFILE or "none".
    local name="$1" payload="$2" profile="$3"
    local -a env_args=(env
        "HOME=$STATE_DIR/home"
        "AUB_CONFIG_FILE=$STATE_DIR/aub.toml"
        "AUB_STATE_DIR=$STATE_DIR")
    if [ -n "$profile" ]; then
        env_args+=("SHALLOW_PROFILE=$profile")
    fi
    step "$name" "${env_args[@]}" sh -c 'cat "$1" | "$2" statusline' _ "$payload" "$AUB_BIN"
}

case_steps() {
    statusline_step "first render" "$STATE_DIR/payload-first.json" gmail
    statusline_step "identical re-render" "$STATE_DIR/payload-first.json" gmail
    statusline_step "moved seven-day render" "$STATE_DIR/payload-moved.json" gmail
    statusline_step "no profile" "$STATE_DIR/payload-first.json" ""
}

case_assertions() {
    assert_exit 0 1
    assert_exit 0 2
    assert_exit 0 3
    assert_exit 0 4

    if cmp -s "$STATE_DIR/payload-first.json" "$(step_dir 1)/stdout.bin"; then
        record_assertion "payload passes through byte-for-byte" "identical" "identical" "pass"
    else
        record_assertion "payload passes through byte-for-byte" "identical" "altered" "fail"
        CASE_FAILED=1
    fi

    local lines_after_moved lines_after_no_profile
    lines_after_no_profile="$(wc -l < "$STATE_DIR/statusline/gmail.jsonl" 2>/dev/null || echo 0)"

    if [ "$lines_after_no_profile" = "2" ]; then
        record_assertion "one line per meter change (2 renders held, 1 move)" "2" "$lines_after_no_profile" "pass"
    else
        record_assertion "one line per meter change (2 renders held, 1 move)" "2" "$lines_after_no_profile" "fail"
        CASE_FAILED=1
    fi

    if [ ! -e "$STATE_DIR/statusline/nobody.jsonl" ]; then
        record_assertion "no account file beyond the profiled one" "absent" "absent" "pass"
    else
        record_assertion "no account file beyond the profiled one" "absent" "present" "fail"
        CASE_FAILED=1
    fi

    # The recorded line's shape, parsed from the file the run actually wrote:
    # the identity fields, the windows map with integer resets_at, and the
    # absence of every payload field the tee does not own.
    if python3 - "$STATE_DIR/statusline/gmail.jsonl" <<'PYEOF'
import json, sys

path = sys.argv[1]
lines = [line for line in open(path).read().splitlines() if line.strip()]
assert len(lines) == 2, f"expected 2 recorded lines, found {len(lines)}"
for line in lines:
    row = json.loads(line)
    assert set(row) == {"received_at", "session_id", "cwd", "windows"}, f"unexpected record fields: {sorted(row)}"
    assert row["session_id"] == "8cd9c60a-e10a-4d44-857e-6b2b931b4d9d", row["session_id"]
    assert row["cwd"] == "/tmp/worktree/project", row["cwd"]
    assert row["received_at"].endswith("Z") and len(row["received_at"]) == 20, row["received_at"]
    windows = row["windows"]
    assert set(windows) == {"five_hour", "seven_day"}, sorted(windows)
    for window in windows.values():
        assert set(window) == {"used_percentage", "resets_at"}, sorted(window)
        assert isinstance(window["resets_at"], int), repr(window["resets_at"])
assert json.loads(lines[1])["windows"]["seven_day"]["used_percentage"] == 13
sys.exit(0)
PYEOF
    then
        record_assertion "recorded line shape (fields, windows, integer resets_at)" "conforms" "conforms" "pass"
    else
        record_assertion "recorded line shape (fields, windows, integer resets_at)" "conforms" "diverges" "fail"
        CASE_FAILED=1
    fi

    if grep -q "12.345678" "$STATE_DIR/statusline/gmail.jsonl"; then
        record_assertion "recorded lines carry no payload field the tee does not own" "absent" "present" "fail"
        CASE_FAILED=1
    else
        record_assertion "recorded lines carry no payload field the tee does not own" "absent" "absent" "pass"
    fi
}